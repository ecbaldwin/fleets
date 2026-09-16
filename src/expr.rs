//! A deliberately small Jinja2-expression evaluator for the `constructed` inventory
//! plugin ([`crate::constructed`]).
//!
//! This is NOT a full Jinja2 engine. It supports the slice of expression syntax that real
//! `compose`/`keyed_groups`/`groups` configs use, and it **errors loudly**
//! ([`ExprError`]) on syntax outside that grammar rather than silently producing a wrong
//! group — see `docs/anomalies.md` §31–32.
//!
//! Supported grammar (lowest to highest precedence), mirroring Jinja2's chain:
//!   - the conditional (ternary) expression `body if cond [else orelse]` (right-associative,
//!     so nested-ternary buckets parse; a false condition with no `else` is Undefined)
//!   - `or`, `and`, `not`
//!   - comparisons `== != < <= > >=`, membership `in` / `not in`,
//!     the tests `is [not] defined` / `is undefined`
//!   - addition/subtraction `+ -`
//!   - string concatenation `~`
//!   - multiplication/division `* / // %`
//!   - power `**`
//!   - unary `-` / `+`
//!   - filters via `|`: `lower upper string int float bool default(x[,bool]) d replace
//!     regex_replace trim capitalize length count first last join abs
//!     ternary(true,false[,none])`
//!   - postfix subscript/slicing `x[i]`, `x[a:b:c]` (Python semantics) and dotted
//!     attribute access `a.b.c`
//!   - `'str'`/`"str"`/int/float/`true`/`false`/`none`/`null` literals, and parentheses
//!
//! Truthiness (for `groups` conditions) follows Jinja2's `{% if %}`: an **undefined**
//! variable is falsey (not an error), as are `none`, `false`, `0`, `""`, `[]`, `{}`.
//!
//! Three internal failure modes are distinguished:
//!   - `Undefined` — a referenced variable (or a missing key/attr/index) is
//!     undefined. Jinja's `{% if %}` treats this as falsey; `default()` reacts to it.
//!   - `Runtime` — a value-level evaluation failure (type mismatch in `+`,
//!     slicing a number, a bad regex, …). ansible's default `strict: false` swallows
//!     these the same way it swallows undefined: the var/group is simply skipped.
//!   - `Unsupported` — syntax outside this grammar (`match`, an unknown
//!     filter, a stray operator). This is the only one that surfaces as a hard error,
//!     even under `strict: false`, because it signals fleets cannot faithfully evaluate
//!     the expression at all (fail-loud over silently-wrong).

use regex::Regex;
use serde_json::{Map, Number, Value};

/// The one error the caller must surface: an expression fleets cannot evaluate faithfully.
#[derive(Debug, Clone)]
pub struct ExprError(pub String);

/// Internal evaluation outcome. See the module docs for the Undefined/Runtime/Unsupported
/// distinction.
enum EvalError {
    Undefined,
    Runtime(String),
    Unsupported(String),
}

/// Evaluate a `keyed_groups` key expression. Returns `Ok(None)` when the key is undefined
/// or fails at runtime (ansible's `strict: false` skips the host for that key), `Ok(Some(v))`
/// with the keying value otherwise, or `Err` for unsupported syntax.
pub fn eval_key(src: &str, vars: &Map<String, Value>) -> Result<Option<Value>, ExprError> {
    match parse(src).and_then(|ast| eval(&ast, vars)) {
        Ok(v) => Ok(Some(v)),
        Err(EvalError::Undefined) | Err(EvalError::Runtime(_)) => Ok(None),
        Err(EvalError::Unsupported(m)) => Err(ExprError(format!("{src:?}: {m}"))),
    }
}

/// Evaluate a `groups` condition to a boolean using Jinja2 `{% if %}` truthiness. An
/// undefined variable (or a runtime failure) evaluates to `false`; only genuinely
/// unsupported syntax errors.
pub fn eval_condition(src: &str, vars: &Map<String, Value>) -> Result<bool, ExprError> {
    match parse(src).and_then(|ast| eval(&ast, vars)) {
        Ok(v) => Ok(truthy(&v)),
        Err(EvalError::Undefined) | Err(EvalError::Runtime(_)) => Ok(false),
        Err(EvalError::Unsupported(m)) => Err(ExprError(format!("{src:?}: {m}"))),
    }
}

/// Evaluate a `compose` expression. Returns `Ok(Some(v))` with the value to assign,
/// `Ok(None)` when the expression failed at runtime/undefined and `strict` is false (the
/// var is simply not set, matching ansible's default), or `Err` for unsupported syntax —
/// and, when `strict` is true, for runtime/undefined failures too.
pub fn eval_compose(
    src: &str,
    vars: &Map<String, Value>,
    strict: bool,
) -> Result<Option<Value>, ExprError> {
    // `parse` only ever fails with `Unsupported`.
    let ast = match parse(src) {
        Ok(ast) => ast,
        Err(EvalError::Unsupported(m)) => return Err(ExprError(format!("{src:?}: {m}"))),
        Err(_) => unreachable!("parse only yields Unsupported"),
    };
    // ansible's templating preserves the native value of a bare `{{ var }}` reference, but
    // a *computed* expression is rendered to text by the (default, non-native) Jinja
    // environment and then lightly re-typed: numbers stay strings, `None` becomes `""`,
    // while booleans and collections are recovered. We mirror that in `coerce_computed`.
    // (Verified differentially; see docs/anomalies.md §32.)
    let bare = is_bare_var(src);
    match eval(&ast, vars) {
        Ok(v) => Ok(Some(if bare { v } else { coerce_computed(v) })),
        Err(EvalError::Unsupported(m)) => Err(ExprError(format!("{src:?}: {m}"))),
        Err(EvalError::Undefined) => {
            if strict {
                Err(ExprError(format!("{src:?}: result is undefined")))
            } else {
                Ok(None)
            }
        }
        Err(EvalError::Runtime(m)) => {
            if strict {
                Err(ExprError(format!("{src:?}: {m}")))
            } else {
                Ok(None)
            }
        }
    }
}

/// ansible preserves native types only for a template that is *textually* a single bare
/// variable reference (`{{ var }}`) — not `(var)`, not `var.attr`, not `var | filter`. The
/// check is therefore on the source text, not the AST (parentheses are transparent in the
/// AST). Literal keywords (`true`/`none`/…) are not bare vars.
fn is_bare_var(src: &str) -> bool {
    let s = src.trim();
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return false;
    }
    !matches!(
        s,
        "true" | "True" | "false" | "False" | "none" | "None" | "null" | "and" | "or" | "not"
    )
}

/// Re-type a computed `compose` result to match ansible's default (non-native Jinja2)
/// templating: a numeric result is rendered as its string form, `None`/null becomes the
/// empty string, and booleans / lists / dicts / strings pass through unchanged.
fn coerce_computed(v: Value) -> Value {
    match v {
        Value::Number(n) => Value::String(n.to_string()),
        Value::Null => Value::String(String::new()),
        other => other,
    }
}

/// Jinja2 `{% if %}` truthiness.
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

// ----------------------------------------------------------------------------------------
// Tokenizer
// ----------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Str(String),
    Int(i64),
    Float(f64),
    Pipe,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    Dot,
    Colon,
    Plus,
    Minus,
    Star,
    StarStar,
    Slash,
    SlashSlash,
    Percent,
    Tilde,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

fn tokenize(src: &str) -> Result<Vec<Tok>, EvalError> {
    let chars: Vec<char> = src.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '|' {
            toks.push(Tok::Pipe);
            i += 1;
        } else if c == '(' {
            toks.push(Tok::LParen);
            i += 1;
        } else if c == ')' {
            toks.push(Tok::RParen);
            i += 1;
        } else if c == '[' {
            toks.push(Tok::LBracket);
            i += 1;
        } else if c == ']' {
            toks.push(Tok::RBracket);
            i += 1;
        } else if c == ',' {
            toks.push(Tok::Comma);
            i += 1;
        } else if c == ':' {
            toks.push(Tok::Colon);
            i += 1;
        } else if c == '~' {
            toks.push(Tok::Tilde);
            i += 1;
        } else if c == '+' {
            toks.push(Tok::Plus);
            i += 1;
        } else if c == '-' {
            toks.push(Tok::Minus);
            i += 1;
        } else if c == '*' {
            if chars.get(i + 1) == Some(&'*') {
                toks.push(Tok::StarStar);
                i += 2;
            } else {
                toks.push(Tok::Star);
                i += 1;
            }
        } else if c == '/' {
            if chars.get(i + 1) == Some(&'/') {
                toks.push(Tok::SlashSlash);
                i += 2;
            } else {
                toks.push(Tok::Slash);
                i += 1;
            }
        } else if c == '%' {
            toks.push(Tok::Percent);
            i += 1;
        } else if c == '.' {
            toks.push(Tok::Dot);
            i += 1;
        } else if c == '=' && chars.get(i + 1) == Some(&'=') {
            toks.push(Tok::Eq);
            i += 2;
        } else if c == '!' && chars.get(i + 1) == Some(&'=') {
            toks.push(Tok::Ne);
            i += 2;
        } else if c == '<' {
            if chars.get(i + 1) == Some(&'=') {
                toks.push(Tok::Le);
                i += 2;
            } else {
                toks.push(Tok::Lt);
                i += 1;
            }
        } else if c == '>' {
            if chars.get(i + 1) == Some(&'=') {
                toks.push(Tok::Ge);
                i += 2;
            } else {
                toks.push(Tok::Gt);
                i += 1;
            }
        } else if c == '\'' || c == '"' {
            let quote = c;
            i += 1;
            let mut s = String::new();
            while i < chars.len() && chars[i] != quote {
                s.push(chars[i]);
                i += 1;
            }
            if i >= chars.len() {
                return Err(EvalError::Unsupported("unterminated string literal".into()));
            }
            i += 1; // closing quote
            toks.push(Tok::Str(s));
        } else if c.is_ascii_digit() {
            let start = i;
            let mut is_float = false;
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                if chars[i] == '.' {
                    // A '.' followed by a digit is a decimal point; otherwise it's member
                    // access on a number (unusual) — stop the number here.
                    if chars.get(i + 1).map(|d| d.is_ascii_digit()) != Some(true) {
                        break;
                    }
                    is_float = true;
                }
                i += 1;
            }
            let text: String = chars[start..i].iter().collect();
            if is_float {
                toks.push(Tok::Float(text.parse().map_err(|_| {
                    EvalError::Unsupported(format!("bad float literal {text:?}"))
                })?));
            } else {
                toks.push(Tok::Int(text.parse().map_err(|_| {
                    EvalError::Unsupported(format!("bad int literal {text:?}"))
                })?));
            }
        } else if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            toks.push(Tok::Ident(chars[start..i].iter().collect()));
        } else {
            return Err(EvalError::Unsupported(format!(
                "unexpected character {c:?}"
            )));
        }
    }
    Ok(toks)
}

// ----------------------------------------------------------------------------------------
// AST + parser (recursive descent)
// ----------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Ast {
    Var(String),
    Lit(Value),
    Attr(Box<Ast>, String),
    Index(Box<Ast>, Box<Ast>),
    Slice(
        Box<Ast>,
        Option<Box<Ast>>,
        Option<Box<Ast>>,
        Option<Box<Ast>>,
    ),
    Filter(Box<Ast>, String, Vec<Ast>),
    Neg(Box<Ast>),
    Bin(BinOp, Box<Ast>, Box<Ast>),
    Not(Box<Ast>),
    And(Box<Ast>, Box<Ast>),
    Or(Box<Ast>, Box<Ast>),
    Cmp(Box<Ast>, CmpOp, Box<Ast>),
    In(Box<Ast>, Box<Ast>, bool),
    Defined(Box<Ast>, bool),
    /// Jinja's conditional (ternary) expression: `body if cond [else orelse]`.
    Cond(Box<Ast>, Box<Ast>, Option<Box<Ast>>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    FloorDiv,
    Mod,
    Pow,
    Concat,
}

#[derive(Debug, Clone, Copy)]
enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

fn parse(src: &str) -> Result<Ast, EvalError> {
    let toks = tokenize(src)?;
    let mut p = Parser { toks, pos: 0 };
    let ast = p.parse_ternary()?;
    if p.pos != p.toks.len() {
        return Err(EvalError::Unsupported(format!(
            "trailing tokens after expression: {:?}",
            &p.toks[p.pos..]
        )));
    }
    Ok(ast)
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn is_kw(&self, kw: &str) -> bool {
        matches!(self.peek(), Some(Tok::Ident(s)) if s == kw)
    }

    fn eat_kw(&mut self, kw: &str) -> bool {
        if self.is_kw(kw) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// The lowest-precedence production — Jinja's conditional (ternary) expression:
    /// `body if cond [else orelse]` (grammar: `or_test ["if" or_test ["else" expression]]`).
    /// The `else` branch is optional (a false condition with no `else` evaluates to
    /// Undefined, as in Jinja) and the recursion on the `else` branch makes the operator
    /// right-associative, so `a if c1 else b if c2 else c` nests as `a if c1 else (b if c2
    /// else c)` — which is what lets a nested-ternary bucket parse. This is the entry point
    /// for a *full* expression; `parse_or` remains the `or_test` sub-production used for the
    /// condition.
    fn parse_ternary(&mut self) -> Result<Ast, EvalError> {
        let body = self.parse_or()?;
        if self.eat_kw("if") {
            let cond = self.parse_or()?;
            let orelse = if self.eat_kw("else") {
                Some(Box::new(self.parse_ternary()?))
            } else {
                None
            };
            return Ok(Ast::Cond(Box::new(cond), Box::new(body), orelse));
        }
        Ok(body)
    }

    fn parse_or(&mut self) -> Result<Ast, EvalError> {
        let mut left = self.parse_and()?;
        while self.eat_kw("or") {
            let right = self.parse_and()?;
            left = Ast::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Ast, EvalError> {
        let mut left = self.parse_not()?;
        while self.eat_kw("and") {
            let right = self.parse_not()?;
            left = Ast::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_not(&mut self) -> Result<Ast, EvalError> {
        if self.eat_kw("not") {
            return Ok(Ast::Not(Box::new(self.parse_not()?)));
        }
        self.parse_comparison()
    }

    fn parse_comparison(&mut self) -> Result<Ast, EvalError> {
        let left = self.parse_addsub()?;

        // `is [not] defined|undefined`
        if self.eat_kw("is") {
            let negated = self.eat_kw("not");
            if self.eat_kw("defined") {
                return Ok(Ast::Defined(Box::new(left), negated));
            }
            if self.eat_kw("undefined") {
                return Ok(Ast::Defined(Box::new(left), !negated));
            }
            return Err(EvalError::Unsupported(
                "only `is [not] defined` / `is undefined` tests are supported".into(),
            ));
        }

        // `in` / `not in`
        if self.is_kw("not")
            && matches!(self.toks.get(self.pos + 1), Some(Tok::Ident(s)) if s == "in")
        {
            self.pos += 2;
            let right = self.parse_addsub()?;
            return Ok(Ast::In(Box::new(left), Box::new(right), true));
        }
        if self.eat_kw("in") {
            let right = self.parse_addsub()?;
            return Ok(Ast::In(Box::new(left), Box::new(right), false));
        }

        // comparison operators
        let op = match self.peek() {
            Some(Tok::Eq) => CmpOp::Eq,
            Some(Tok::Ne) => CmpOp::Ne,
            Some(Tok::Lt) => CmpOp::Lt,
            Some(Tok::Le) => CmpOp::Le,
            Some(Tok::Gt) => CmpOp::Gt,
            Some(Tok::Ge) => CmpOp::Ge,
            _ => return Ok(left),
        };
        self.pos += 1;
        let right = self.parse_addsub()?;
        Ok(Ast::Cmp(Box::new(left), op, Box::new(right)))
    }

    fn parse_addsub(&mut self) -> Result<Ast, EvalError> {
        let mut left = self.parse_concat()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Plus) => BinOp::Add,
                Some(Tok::Minus) => BinOp::Sub,
                _ => break,
            };
            self.pos += 1;
            let right = self.parse_concat()?;
            left = Ast::Bin(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_concat(&mut self) -> Result<Ast, EvalError> {
        let mut left = self.parse_muldiv()?;
        while matches!(self.peek(), Some(Tok::Tilde)) {
            self.pos += 1;
            let right = self.parse_muldiv()?;
            left = Ast::Bin(BinOp::Concat, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_muldiv(&mut self) -> Result<Ast, EvalError> {
        let mut left = self.parse_pow()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Star) => BinOp::Mul,
                Some(Tok::Slash) => BinOp::Div,
                Some(Tok::SlashSlash) => BinOp::FloorDiv,
                Some(Tok::Percent) => BinOp::Mod,
                _ => break,
            };
            self.pos += 1;
            let right = self.parse_pow()?;
            left = Ast::Bin(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_pow(&mut self) -> Result<Ast, EvalError> {
        let left = self.parse_unary()?;
        // `**` is right-associative.
        if matches!(self.peek(), Some(Tok::StarStar)) {
            self.pos += 1;
            let right = self.parse_pow()?;
            return Ok(Ast::Bin(BinOp::Pow, Box::new(left), Box::new(right)));
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Ast, EvalError> {
        if matches!(self.peek(), Some(Tok::Minus)) {
            self.pos += 1;
            return Ok(Ast::Neg(Box::new(self.parse_unary()?)));
        }
        if matches!(self.peek(), Some(Tok::Plus)) {
            self.pos += 1; // unary plus is a no-op
            return self.parse_unary();
        }
        self.parse_filter()
    }

    fn parse_filter(&mut self) -> Result<Ast, EvalError> {
        let mut expr = self.parse_postfix()?;
        while matches!(self.peek(), Some(Tok::Pipe)) {
            self.pos += 1;
            let name = match self.peek() {
                Some(Tok::Ident(s)) => s.clone(),
                _ => {
                    return Err(EvalError::Unsupported(
                        "expected filter name after `|`".into(),
                    ));
                }
            };
            // Reject unsupported filters at parse time so the divergence fires on the
            // expression itself, independent of whether the operand happens to be undefined.
            if !KNOWN_FILTERS.contains(&name.as_str()) {
                return Err(EvalError::Unsupported(format!(
                    "unsupported filter `{name}`"
                )));
            }
            self.pos += 1;
            let mut args = Vec::new();
            if matches!(self.peek(), Some(Tok::LParen)) {
                self.pos += 1;
                if !matches!(self.peek(), Some(Tok::RParen)) {
                    loop {
                        args.push(self.parse_ternary()?);
                        if matches!(self.peek(), Some(Tok::Comma)) {
                            self.pos += 1;
                            continue;
                        }
                        break;
                    }
                }
                if !matches!(self.peek(), Some(Tok::RParen)) {
                    return Err(EvalError::Unsupported(
                        "expected `)` after filter args".into(),
                    ));
                }
                self.pos += 1;
            }
            expr = Ast::Filter(Box::new(expr), name, args);
        }
        Ok(expr)
    }

    /// Postfix attribute access (`.name`) and subscript/slicing (`[i]`, `[a:b:c]`).
    fn parse_postfix(&mut self) -> Result<Ast, EvalError> {
        let mut expr = self.parse_primary()?;
        loop {
            match self.peek() {
                Some(Tok::Dot) => {
                    self.pos += 1;
                    match self.peek().cloned() {
                        Some(Tok::Ident(seg)) => {
                            self.pos += 1;
                            expr = Ast::Attr(Box::new(expr), seg);
                        }
                        _ => {
                            return Err(EvalError::Unsupported(
                                "expected identifier after `.`".into(),
                            ));
                        }
                    }
                }
                Some(Tok::LBracket) => {
                    self.pos += 1;
                    expr = self.parse_subscript(expr)?;
                }
                _ => break,
            }
        }
        Ok(expr)
    }

    /// Parse the contents of a `[...]` after the opening bracket has been consumed.
    fn parse_subscript(&mut self, target: Ast) -> Result<Ast, EvalError> {
        // A leading colon means a slice with no start.
        let start = if matches!(self.peek(), Some(Tok::Colon)) {
            None
        } else {
            Some(Box::new(self.parse_ternary()?))
        };

        if matches!(self.peek(), Some(Tok::Colon)) {
            // slice: target[start:stop:step]
            self.pos += 1;
            let stop = if matches!(self.peek(), Some(Tok::Colon) | Some(Tok::RBracket)) {
                None
            } else {
                Some(Box::new(self.parse_ternary()?))
            };
            let step = if matches!(self.peek(), Some(Tok::Colon)) {
                self.pos += 1;
                if matches!(self.peek(), Some(Tok::RBracket)) {
                    None
                } else {
                    Some(Box::new(self.parse_ternary()?))
                }
            } else {
                None
            };
            self.expect_rbracket()?;
            Ok(Ast::Slice(Box::new(target), start, stop, step))
        } else {
            // plain index
            let idx = start.ok_or_else(|| EvalError::Unsupported("empty subscript".into()))?;
            self.expect_rbracket()?;
            Ok(Ast::Index(Box::new(target), idx))
        }
    }

    fn expect_rbracket(&mut self) -> Result<(), EvalError> {
        if !matches!(self.peek(), Some(Tok::RBracket)) {
            return Err(EvalError::Unsupported("expected `]`".into()));
        }
        self.pos += 1;
        Ok(())
    }

    fn parse_primary(&mut self) -> Result<Ast, EvalError> {
        match self.peek().cloned() {
            Some(Tok::LParen) => {
                self.pos += 1;
                let inner = self.parse_ternary()?;
                if !matches!(self.peek(), Some(Tok::RParen)) {
                    return Err(EvalError::Unsupported("expected `)`".into()));
                }
                self.pos += 1;
                Ok(inner)
            }
            Some(Tok::Str(s)) => {
                self.pos += 1;
                Ok(Ast::Lit(Value::String(s)))
            }
            Some(Tok::Int(n)) => {
                self.pos += 1;
                Ok(Ast::Lit(Value::Number(n.into())))
            }
            Some(Tok::Float(f)) => {
                self.pos += 1;
                Ok(Ast::Lit(
                    Number::from_f64(f)
                        .map(Value::Number)
                        .unwrap_or(Value::Null),
                ))
            }
            Some(Tok::Ident(s)) => {
                self.pos += 1;
                match s.as_str() {
                    "true" | "True" => Ok(Ast::Lit(Value::Bool(true))),
                    "false" | "False" => Ok(Ast::Lit(Value::Bool(false))),
                    "none" | "None" | "null" => Ok(Ast::Lit(Value::Null)),
                    _ => Ok(Ast::Var(s)),
                }
            }
            other => Err(EvalError::Unsupported(format!(
                "unexpected token {other:?}"
            ))),
        }
    }
}

const KNOWN_FILTERS: &[&str] = &[
    "lower",
    "upper",
    "string",
    "int",
    "float",
    "bool",
    "default",
    "d",
    "replace",
    "regex_replace",
    "trim",
    "capitalize",
    "length",
    "count",
    "first",
    "last",
    "join",
    "abs",
    "ternary",
];

// ----------------------------------------------------------------------------------------
// Evaluator
// ----------------------------------------------------------------------------------------

fn eval(ast: &Ast, vars: &Map<String, Value>) -> Result<Value, EvalError> {
    match ast {
        Ast::Lit(v) => Ok(v.clone()),
        Ast::Var(name) => match vars.get(name) {
            Some(v) => Ok(v.clone()),
            None => Err(EvalError::Undefined),
        },
        Ast::Attr(e, key) => {
            let base = eval(e, vars)?;
            match base {
                Value::Object(o) => o.get(key).cloned().ok_or(EvalError::Undefined),
                _ => Err(EvalError::Undefined),
            }
        }
        Ast::Index(e, idx) => {
            let base = eval(e, vars)?;
            let i = eval(idx, vars)?;
            index(&base, &i)
        }
        Ast::Slice(e, start, stop, step) => {
            let base = eval(e, vars)?;
            let s = opt_eval(start, vars)?;
            let t = opt_eval(stop, vars)?;
            let p = opt_eval(step, vars)?;
            slice(&base, s, t, p)
        }
        Ast::Neg(e) => match eval(e, vars)? {
            Value::Number(n) => negate(&n),
            other => Err(EvalError::Runtime(format!(
                "cannot negate {}",
                type_name(&other)
            ))),
        },
        Ast::Bin(op, a, b) => {
            let l = eval(a, vars)?;
            let r = eval(b, vars)?;
            bin_op(*op, &l, &r)
        }
        Ast::Not(e) => {
            let v = match eval(e, vars) {
                Ok(v) => truthy(&v),
                Err(EvalError::Undefined) => false,
                Err(e) => return Err(e),
            };
            Ok(Value::Bool(!v))
        }
        Ast::And(a, b) => {
            let av = truthy_or_undef(a, vars)?;
            if !av {
                return Ok(Value::Bool(false));
            }
            Ok(Value::Bool(truthy_or_undef(b, vars)?))
        }
        Ast::Or(a, b) => {
            if truthy_or_undef(a, vars)? {
                return Ok(Value::Bool(true));
            }
            Ok(Value::Bool(truthy_or_undef(b, vars)?))
        }
        Ast::Defined(e, want_undefined) => {
            let is_undef = match eval(e, vars) {
                Err(EvalError::Undefined) => true,
                // An unsupported error inside the operand must still surface.
                Err(e @ EvalError::Unsupported(_)) => return Err(e),
                // A runtime failure means the value existed but the operation failed;
                // ansible's `is defined` reports the value as defined in that case.
                _ => false,
            };
            Ok(Value::Bool(if *want_undefined {
                is_undef
            } else {
                !is_undef
            }))
        }
        Ast::Cmp(a, op, b) => {
            let l = eval(a, vars)?;
            let r = eval(b, vars)?;
            Ok(Value::Bool(compare(&l, *op, &r)))
        }
        Ast::In(a, b, negated) => {
            let l = eval(a, vars)?;
            let r = eval(b, vars)?;
            let found = contains(&r, &l);
            Ok(Value::Bool(found != *negated))
        }
        Ast::Filter(e, name, args) => apply_filter(e, name, args, vars),
        Ast::Cond(cond, body, orelse) => {
            // Jinja's `body if cond else orelse`. The condition's truthiness follows
            // `{% if %}`, so an *undefined* condition is falsey (not an error); a genuine
            // runtime/unsupported failure in the condition still propagates. Only the taken
            // branch is evaluated, so an undefined/erroring *untaken* branch never fails the
            // host — which is what lets a nested-ternary bucket fall through cleanly. With no
            // `else`, a false condition yields Undefined (Jinja's behaviour).
            let take = match eval(cond, vars) {
                Ok(v) => truthy(&v),
                Err(EvalError::Undefined) => false,
                Err(other) => return Err(other),
            };
            if take {
                eval(body, vars)
            } else {
                match orelse {
                    Some(e) => eval(e, vars),
                    None => Err(EvalError::Undefined),
                }
            }
        }
    }
}

fn opt_eval(e: &Option<Box<Ast>>, vars: &Map<String, Value>) -> Result<Option<Value>, EvalError> {
    match e {
        Some(a) => Ok(Some(eval(a, vars)?)),
        None => Ok(None),
    }
}

/// Evaluate a sub-expression to a bool for `and`/`or`, mapping undefined/runtime to false.
fn truthy_or_undef(ast: &Ast, vars: &Map<String, Value>) -> Result<bool, EvalError> {
    match eval(ast, vars) {
        Ok(v) => Ok(truthy(&v)),
        Err(EvalError::Undefined) | Err(EvalError::Runtime(_)) => Ok(false),
        Err(e) => Err(e),
    }
}

// ----------------------------------------------------------------------------------------
// Indexing & slicing (Python / Jinja2 `getitem` semantics)
// ----------------------------------------------------------------------------------------

/// `container[idx]`. Jinja's `getitem` returns Undefined for any miss (bad key, out of
/// range, wrong type), so we map every failure to `Undefined` rather than erroring.
fn index(container: &Value, idx: &Value) -> Result<Value, EvalError> {
    match container {
        Value::Array(a) => {
            let i = idx.as_i64().ok_or(EvalError::Undefined)?;
            let n = a.len() as i64;
            let j = if i < 0 { i + n } else { i };
            if j < 0 || j >= n {
                Err(EvalError::Undefined)
            } else {
                Ok(a[j as usize].clone())
            }
        }
        Value::String(s) => {
            let chars: Vec<char> = s.chars().collect();
            let i = idx.as_i64().ok_or(EvalError::Undefined)?;
            let n = chars.len() as i64;
            let j = if i < 0 { i + n } else { i };
            if j < 0 || j >= n {
                Err(EvalError::Undefined)
            } else {
                Ok(Value::String(chars[j as usize].to_string()))
            }
        }
        Value::Object(o) => {
            let k = idx.as_str().ok_or(EvalError::Undefined)?;
            o.get(k).cloned().ok_or(EvalError::Undefined)
        }
        _ => Err(EvalError::Undefined),
    }
}

/// `container[start:stop:step]` with Python semantics. Slicing a non-sequence is Undefined
/// (Jinja `getitem`); a zero step is a runtime error (Python `ValueError`).
fn slice(
    container: &Value,
    start: Option<Value>,
    stop: Option<Value>,
    step: Option<Value>,
) -> Result<Value, EvalError> {
    let as_opt_i64 = |v: &Option<Value>| -> Result<Option<i64>, EvalError> {
        match v {
            None => Ok(None),
            Some(Value::Null) => Ok(None),
            Some(x) => x.as_i64().map(Some).ok_or(EvalError::Undefined),
        }
    };
    let start = as_opt_i64(&start)?;
    let stop = as_opt_i64(&stop)?;
    let step = as_opt_i64(&step)?;

    match container {
        Value::String(s) => {
            let chars: Vec<char> = s.chars().collect();
            let idxs = slice_indices(chars.len() as i64, start, stop, step)?;
            Ok(Value::String(idxs.into_iter().map(|i| chars[i]).collect()))
        }
        Value::Array(a) => {
            let idxs = slice_indices(a.len() as i64, start, stop, step)?;
            Ok(Value::Array(
                idxs.into_iter().map(|i| a[i].clone()).collect(),
            ))
        }
        _ => Err(EvalError::Undefined),
    }
}

/// Reproduce CPython's `slice.indices` + iteration to yield the selected indices.
fn slice_indices(
    len: i64,
    start: Option<i64>,
    stop: Option<i64>,
    step: Option<i64>,
) -> Result<Vec<usize>, EvalError> {
    let step = step.unwrap_or(1);
    if step == 0 {
        return Err(EvalError::Runtime("slice step cannot be zero".into()));
    }
    let (lower, upper) = if step < 0 { (-1, len - 1) } else { (0, len) };

    let clamp_start = |v: i64| {
        if v < 0 {
            (v + len).max(lower)
        } else {
            v.min(upper)
        }
    };
    let clamp_stop = clamp_start;

    let start = match start {
        None => {
            if step < 0 {
                upper
            } else {
                lower
            }
        }
        Some(v) => clamp_start(v),
    };
    let stop = match stop {
        None => {
            if step < 0 {
                lower
            } else {
                upper
            }
        }
        Some(v) => clamp_stop(v),
    };

    let mut out = Vec::new();
    let mut i = start;
    if step > 0 {
        while i < stop {
            if i >= 0 && i < len {
                out.push(i as usize);
            }
            i += step;
        }
    } else {
        while i > stop {
            if i >= 0 && i < len {
                out.push(i as usize);
            }
            i += step;
        }
    }
    Ok(out)
}

// ----------------------------------------------------------------------------------------
// Binary operators (Python / Jinja2 semantics)
// ----------------------------------------------------------------------------------------

enum Num {
    I(i64),
    F(f64),
}

fn as_num(v: &Value) -> Option<Num> {
    match v {
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(Num::I(i))
            } else {
                n.as_f64().map(Num::F)
            }
        }
        _ => None,
    }
}

fn f64_of(n: &Num) -> f64 {
    match n {
        Num::I(i) => *i as f64,
        Num::F(f) => *f,
    }
}

fn num_value(f: f64) -> Result<Value, EvalError> {
    Number::from_f64(f)
        .map(Value::Number)
        .ok_or_else(|| EvalError::Runtime("non-finite numeric result".into()))
}

fn int_value(i: i64) -> Value {
    Value::Number(i.into())
}

fn negate(n: &Number) -> Result<Value, EvalError> {
    if let Some(i) = n.as_i64() {
        Ok(int_value(-i))
    } else if let Some(f) = n.as_f64() {
        num_value(-f)
    } else {
        Err(EvalError::Runtime("cannot negate number".into()))
    }
}

fn bin_op(op: BinOp, l: &Value, r: &Value) -> Result<Value, EvalError> {
    use BinOp::*;

    // `~` always stringifies both operands and concatenates.
    if op == Concat {
        return Ok(Value::String(format!("{}{}", jinja_str(l)?, jinja_str(r)?)));
    }

    // `+` doubles as string and list concatenation.
    if op == Add {
        match (l, r) {
            (Value::String(a), Value::String(b)) => return Ok(Value::String(format!("{a}{b}"))),
            (Value::Array(a), Value::Array(b)) => {
                let mut v = a.clone();
                v.extend(b.clone());
                return Ok(Value::Array(v));
            }
            _ => {}
        }
    }

    // `*` doubles as repetition (str*int, list*int).
    if op == Mul {
        match (l, r) {
            (Value::String(s), Value::Number(n)) | (Value::Number(n), Value::String(s)) => {
                if let Some(c) = n.as_i64() {
                    return Ok(Value::String(if c > 0 {
                        s.repeat(c as usize)
                    } else {
                        String::new()
                    }));
                }
            }
            (Value::Array(a), Value::Number(n)) | (Value::Number(n), Value::Array(a)) => {
                if let Some(c) = n.as_i64() {
                    let mut out = Vec::new();
                    for _ in 0..c.max(0) {
                        out.extend(a.clone());
                    }
                    return Ok(Value::Array(out));
                }
            }
            _ => {}
        }
    }

    let (ln, rn) = match (as_num(l), as_num(r)) {
        (Some(a), Some(b)) => (a, b),
        _ => {
            return Err(EvalError::Runtime(format!(
                "unsupported operands for arithmetic: {} and {}",
                type_name(l),
                type_name(r)
            )));
        }
    };

    let both_int = matches!((&ln, &rn), (Num::I(_), Num::I(_)));
    let (lf, rf) = (f64_of(&ln), f64_of(&rn));

    match op {
        Add => {
            if both_int {
                Ok(int_value(lf as i64 + rf as i64))
            } else {
                num_value(lf + rf)
            }
        }
        Sub => {
            if both_int {
                Ok(int_value(lf as i64 - rf as i64))
            } else {
                num_value(lf - rf)
            }
        }
        Mul => {
            if both_int {
                Ok(int_value(lf as i64 * rf as i64))
            } else {
                num_value(lf * rf)
            }
        }
        Div => {
            // Python3 `/` is always float division.
            if rf == 0.0 {
                Err(EvalError::Runtime("division by zero".into()))
            } else {
                num_value(lf / rf)
            }
        }
        FloorDiv => {
            if rf == 0.0 {
                Err(EvalError::Runtime("division by zero".into()))
            } else if both_int {
                Ok(int_value((lf as i64).div_euclid(rf as i64)))
            } else {
                num_value((lf / rf).floor())
            }
        }
        Mod => {
            if rf == 0.0 {
                Err(EvalError::Runtime("modulo by zero".into()))
            } else if both_int {
                Ok(int_value((lf as i64).rem_euclid(rf as i64)))
            } else {
                // Python modulo takes the sign of the divisor.
                let m = lf - rf * (lf / rf).floor();
                num_value(m)
            }
        }
        Pow => {
            if both_int && rf >= 0.0 {
                Ok(int_value((lf as i64).pow(rf as u32)))
            } else {
                num_value(lf.powf(rf))
            }
        }
        // `Concat` returns early above; nothing else reaches here.
        Concat => unreachable!(),
    }
}

// ----------------------------------------------------------------------------------------
// Comparisons & membership
// ----------------------------------------------------------------------------------------

fn compare(l: &Value, op: CmpOp, r: &Value) -> bool {
    use CmpOp::*;
    match op {
        Eq => l == r,
        Ne => l != r,
        Lt | Le | Gt | Ge => {
            let ord = match (l, r) {
                (Value::Number(a), Value::Number(b)) => a.as_f64().partial_cmp(&b.as_f64()),
                (Value::String(a), Value::String(b)) => Some(a.cmp(b)),
                _ => None,
            };
            matches!(
                (ord, op),
                (Some(std::cmp::Ordering::Less), Lt | Le)
                    | (Some(std::cmp::Ordering::Equal), Le | Ge)
                    | (Some(std::cmp::Ordering::Greater), Gt | Ge)
            )
        }
    }
}

/// `needle in haystack` for arrays (membership), strings (substring), object (key) lookups.
fn contains(haystack: &Value, needle: &Value) -> bool {
    match haystack {
        Value::Array(a) => a.contains(needle),
        Value::String(s) => needle.as_str().map(|n| s.contains(n)).unwrap_or(false),
        Value::Object(o) => needle.as_str().map(|n| o.contains_key(n)).unwrap_or(false),
        _ => false,
    }
}

// ----------------------------------------------------------------------------------------
// Filters
// ----------------------------------------------------------------------------------------

fn apply_filter(
    e: &Ast,
    name: &str,
    args: &[Ast],
    vars: &Map<String, Value>,
) -> Result<Value, EvalError> {
    // `default` must see the undefined-ness of its operand, so handle it before eval.
    if name == "default" || name == "d" {
        let fallback = || match args.first() {
            Some(a) => eval(a, vars),
            None => Ok(Value::String(String::new())),
        };
        return match eval(e, vars) {
            Ok(v) => {
                // Optional 2nd arg: when truthy, also fall back on a defined-but-falsey value.
                let on_falsey = match args.get(1) {
                    Some(a) => truthy(&eval(a, vars)?),
                    None => false,
                };
                if on_falsey && !truthy(&v) {
                    fallback()
                } else {
                    Ok(v)
                }
            }
            Err(EvalError::Undefined) => fallback(),
            Err(other) => Err(other),
        };
    }

    // `ternary` selects one of its branches by the truthiness of its operand. ansible's
    // filter is `value | ternary(true_val, false_val, none_val=None)`:
    //   - `value is None` and a `none_val` is supplied (and not itself None) -> `none_val`
    //   - else truthy `value` -> `true_val`, falsey -> `false_val`.
    // An *undefined* operand is distinct from `None`: Jinja's `bool(Undefined)` is false, so
    // it takes the `false_val` branch (it never reaches the `none_val` case). We evaluate only
    // the selected branch — this lets nested ternaries
    // (`a | ternary('x', b | ternary('y', 'z'))`) compose, and matches Jinja passing an
    // unused undefined branch around harmlessly rather than forcing it.
    if name == "ternary" {
        if args.len() < 2 || args.len() > 3 {
            return Err(EvalError::Unsupported(
                "filter `ternary` takes 2 or 3 arguments (true, false[, none])".into(),
            ));
        }
        // `None` = operand undefined (Jinja Undefined), `Some(Null)` = an actual `none` value.
        let val = match eval(e, vars) {
            Ok(v) => Some(v),
            Err(EvalError::Undefined) => None,
            Err(other) => return Err(other),
        };
        if matches!(val, Some(Value::Null)) {
            if let Some(none_arg) = args.get(2) {
                let none_val = eval(none_arg, vars)?;
                if !matches!(none_val, Value::Null) {
                    return Ok(none_val);
                }
            }
        }
        let take_true = val.as_ref().map(truthy).unwrap_or(false);
        return eval(if take_true { &args[0] } else { &args[1] }, vars);
    }

    let v = eval(e, vars)?;
    let arg_str = |i: usize| -> Result<String, EvalError> {
        match args.get(i) {
            Some(a) => jinja_str(&eval(a, vars)?),
            None => Err(EvalError::Runtime(format!(
                "filter `{name}` is missing a required argument"
            ))),
        }
    };

    match name {
        "lower" => Ok(Value::String(as_string(&v)?.to_lowercase())),
        "upper" => Ok(Value::String(as_string(&v)?.to_uppercase())),
        "string" => Ok(Value::String(as_string(&v)?)),
        "int" => {
            let n = match &v {
                Value::Number(n) => n.as_i64().unwrap_or(0),
                Value::String(s) => s.trim().parse::<i64>().unwrap_or(0),
                Value::Bool(b) => *b as i64,
                _ => 0,
            };
            Ok(int_value(n))
        }
        "float" => {
            let f = match &v {
                Value::Number(n) => n.as_f64().unwrap_or(0.0),
                Value::String(s) => s.trim().parse::<f64>().unwrap_or(0.0),
                Value::Bool(b) => *b as i64 as f64,
                _ => 0.0,
            };
            num_value(f)
        }
        "bool" => Ok(Value::Bool(ansible_bool(&v))),
        "abs" => match &v {
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Ok(int_value(i.abs()))
                } else if let Some(f) = n.as_f64() {
                    num_value(f.abs())
                } else {
                    Err(EvalError::Runtime("abs of non-number".into()))
                }
            }
            _ => Err(EvalError::Runtime("abs of non-number".into())),
        },
        "replace" => {
            let old = arg_str(0)?;
            let new = arg_str(1)?;
            Ok(Value::String(as_string(&v)?.replace(&old, &new)))
        }
        "regex_replace" => {
            let pat = arg_str(0)?;
            let rep = arg_str(1)?;
            let re = Regex::new(&pat)
                .map_err(|e| EvalError::Runtime(format!("invalid regex {pat:?}: {e}")))?;
            let rep = translate_replacement(&rep);
            Ok(Value::String(
                re.replace_all(&as_string(&v)?, rep.as_str()).into_owned(),
            ))
        }
        "trim" => {
            let s = as_string(&v)?;
            let trimmed = match args.first() {
                Some(a) => {
                    let chars = jinja_str(&eval(a, vars)?)?;
                    let set: Vec<char> = chars.chars().collect();
                    s.trim_matches(|c| set.contains(&c)).to_string()
                }
                None => s.trim().to_string(),
            };
            Ok(Value::String(trimmed))
        }
        "capitalize" => {
            let s = as_string(&v)?;
            let mut out = String::new();
            for (i, c) in s.chars().enumerate() {
                if i == 0 {
                    out.extend(c.to_uppercase());
                } else {
                    out.extend(c.to_lowercase());
                }
            }
            Ok(Value::String(out))
        }
        "length" | "count" => match &v {
            Value::String(s) => Ok(int_value(s.chars().count() as i64)),
            Value::Array(a) => Ok(int_value(a.len() as i64)),
            Value::Object(o) => Ok(int_value(o.len() as i64)),
            _ => Err(EvalError::Runtime("object has no length".into())),
        },
        "first" => match &v {
            Value::Array(a) => a.first().cloned().ok_or(EvalError::Undefined),
            Value::String(s) => s
                .chars()
                .next()
                .map(|c| Value::String(c.to_string()))
                .ok_or(EvalError::Undefined),
            _ => Err(EvalError::Runtime("first of non-sequence".into())),
        },
        "last" => match &v {
            Value::Array(a) => a.last().cloned().ok_or(EvalError::Undefined),
            Value::String(s) => s
                .chars()
                .last()
                .map(|c| Value::String(c.to_string()))
                .ok_or(EvalError::Undefined),
            _ => Err(EvalError::Runtime("last of non-sequence".into())),
        },
        "join" => {
            let sep = match args.first() {
                Some(a) => jinja_str(&eval(a, vars)?)?,
                None => String::new(),
            };
            match &v {
                Value::Array(a) => {
                    let parts: Result<Vec<String>, EvalError> = a.iter().map(jinja_str).collect();
                    Ok(Value::String(parts?.join(&sep)))
                }
                _ => Err(EvalError::Runtime("join of non-list".into())),
            }
        }
        other => Err(EvalError::Unsupported(format!(
            "unsupported filter `{other}`"
        ))),
    }
}

/// Translate Python `re.sub` replacement backreferences (`\1`, `\g<1>`) to the Rust
/// `regex` crate's `$1` / `${1}` form, and escape literal `$`. Best-effort: exotic Python
/// replacement syntax may still diverge (documented in `docs/anomalies.md`).
fn translate_replacement(rep: &str) -> String {
    let chars: Vec<char> = rep.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '$' {
            out.push_str("$$"); // literal dollar in the Rust replacement syntax
            i += 1;
        } else if c == '\\' && i + 1 < chars.len() {
            let next = chars[i + 1];
            if next.is_ascii_digit() {
                // \1 -> ${1} (brace form avoids greedy multi-digit grabbing)
                let mut j = i + 1;
                let mut num = String::new();
                while j < chars.len() && chars[j].is_ascii_digit() {
                    num.push(chars[j]);
                    j += 1;
                }
                out.push_str(&format!("${{{num}}}"));
                i = j;
            } else if next == 'g' && chars.get(i + 2) == Some(&'<') {
                // \g<name> -> ${name}
                let mut j = i + 3;
                let mut name = String::new();
                while j < chars.len() && chars[j] != '>' {
                    name.push(chars[j]);
                    j += 1;
                }
                out.push_str(&format!("${{{name}}}"));
                i = j + 1; // skip '>'
            } else {
                out.push(next);
                i += 2;
            }
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

/// Stringify a scalar for string-oriented filters. Containers are unsupported here.
fn as_string(v: &Value) -> Result<String, EvalError> {
    match v {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Bool(b) => Ok(if *b { "True".into() } else { "False".into() }),
        Value::Null => Ok("None".into()),
        _ => Err(EvalError::Runtime(
            "string operation applied to a list/dict".into(),
        )),
    }
}

/// Jinja's `str()` coercion (used by `~` and string-valued filter args). Same as
/// [`as_string`] for scalars; containers are a runtime error (their Python repr is not
/// reproduced).
fn jinja_str(v: &Value) -> Result<String, EvalError> {
    as_string(v)
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "none",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// ansible's `boolean()` semantics for the `| bool` filter.
fn ansible_bool(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Value::String(s) => matches!(
            s.to_ascii_lowercase().as_str(),
            "true" | "yes" | "on" | "1" | "t" | "y"
        ),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn vars() -> Map<String, Value> {
        json!({
            "lockdown": "Restricted",
            "region": "us-east-01",
            "zone": "us-east-01a",
            "tier": "gold",
            "count": 3,
            "enabled": true,
            "facts": {"os": "linux"},
            "roles": ["db", "cache"],
        })
        .as_object()
        .unwrap()
        .clone()
    }

    fn key(s: &str) -> Option<Value> {
        eval_key(s, &vars()).unwrap()
    }
    fn cond(s: &str) -> bool {
        eval_condition(s, &vars()).unwrap()
    }
    fn comp(s: &str) -> Option<Value> {
        eval_compose(s, &vars(), false).unwrap()
    }

    #[test]
    fn bare_var_and_filters() {
        assert_eq!(key("lockdown"), Some(json!("Restricted")));
        assert_eq!(key("lockdown | lower"), Some(json!("restricted")));
        assert_eq!(key("region | upper"), Some(json!("US-EAST-01")));
        assert_eq!(key("count | string"), Some(json!("3")));
    }

    #[test]
    fn dotted_path() {
        assert_eq!(key("facts.os"), Some(json!("linux")));
        assert_eq!(key("facts.missing"), None);
    }

    #[test]
    fn undefined_key_is_none() {
        assert_eq!(key("nope"), None);
        assert_eq!(key("nope | lower"), None);
        assert_eq!(key("nope | default('fallback')"), Some(json!("fallback")));
    }

    #[test]
    fn conditions() {
        assert!(cond("lockdown is defined"));
        assert!(!cond("nope is defined"));
        assert!(cond("nope is not defined"));
        assert!(cond("nope is undefined"));
        assert!(cond("tier == 'gold'"));
        assert!(!cond("tier == 'silver'"));
        assert!(cond("tier != 'silver'"));
        assert!(cond("count > 1"));
        assert!(!cond("count > 5"));
        assert!(cond("'db' in roles"));
        assert!(cond("'x' not in roles"));
        assert!(cond("enabled and tier is defined"));
        assert!(cond("nope is defined or count == 3"));
        assert!(cond("not (nope is defined)"));
        assert!(cond("lockdown != 'disabled'"));
    }

    #[test]
    fn undefined_in_condition_is_false_not_error() {
        assert!(!cond("nope"));
        assert!(!cond("nope and enabled"));
    }

    #[test]
    fn arithmetic_and_concat() {
        // `key()` exposes the raw evaluated value (no compose coercion).
        assert_eq!(key("count + 1"), Some(json!(4)));
        assert_eq!(key("count - 5"), Some(json!(-2)));
        assert_eq!(key("count * 2"), Some(json!(6)));
        assert_eq!(key("count / 2"), Some(json!(1.5)));
        assert_eq!(key("count // 2"), Some(json!(1)));
        assert_eq!(key("count % 2"), Some(json!(1)));
        assert_eq!(key("count ** 2"), Some(json!(9)));
        assert_eq!(key("-count"), Some(json!(-3)));
        // string concat with + and ~
        assert_eq!(key("region + 'a'"), Some(json!("us-east-01a")));
        assert_eq!(key("'r' ~ count"), Some(json!("r3")));
        // filter binds tighter than +
        assert_eq!(key("'A' + tier | upper"), Some(json!("AGOLD")));
    }

    #[test]
    fn slicing_and_indexing() {
        assert_eq!(key("zone[:-1]"), Some(json!("us-east-01")));
        assert_eq!(key("zone[0]"), Some(json!("u")));
        assert_eq!(key("zone[-1]"), Some(json!("a")));
        assert_eq!(key("zone[0:2]"), Some(json!("us")));
        assert_eq!(key("roles[1]"), Some(json!("cache")));
        assert_eq!(key("roles[::-1]"), Some(json!(["cache", "db"])));
        // out-of-range index is undefined -> skipped under strict:false
        assert_eq!(key("roles[9]"), None);
    }

    #[test]
    fn broadened_filters() {
        assert_eq!(key("zone | replace('-', '_')"), Some(json!("us_east_01a")));
        assert_eq!(
            key("zone | regex_replace('[0-9]+', 'N')"),
            Some(json!("us-east-Na"))
        );
        assert_eq!(key("'  hi  ' | trim"), Some(json!("hi")));
        assert_eq!(key("tier | capitalize"), Some(json!("Gold")));
        assert_eq!(key("roles | length"), Some(json!(2)));
        assert_eq!(key("roles | first"), Some(json!("db")));
        assert_eq!(key("roles | last"), Some(json!("cache")));
        assert_eq!(key("roles | join('-')"), Some(json!("db-cache")));
        assert_eq!(key("count | float"), Some(json!(3.0)));
        // filter binds tighter than unary minus: `-count | abs` == `-(count|abs)`
        assert_eq!(key("-count | abs"), Some(json!(-3)));
        assert_eq!(key("(0 - count) | abs"), Some(json!(3)));
    }

    #[test]
    fn conditional_expression() {
        // the crisp repro: `'a' if (x == 5) else 'b'` — all three forms
        assert_eq!(key("'a' if (count == 3) else 'b'"), Some(json!("a")));
        assert_eq!(key("'a' if (count == 5) else 'b'"), Some(json!("b")));
        assert_eq!(key("'a' if count == 3 else 'b'"), Some(json!("a")));
        // condition off a bare/derived value
        assert_eq!(key("'on' if enabled else 'off'"), Some(json!("on")));
        // an undefined condition is falsey (not an error) -> else branch
        assert_eq!(key("'a' if nope else 'b'"), Some(json!("b")));
        // only the taken branch is evaluated: an undefined untaken branch doesn't fail
        assert_eq!(key("'a' if count == 3 else nope"), Some(json!("a")));
        assert_eq!(key("nope if count == 5 else 'b'"), Some(json!("b")));
        // no `else`: false condition yields Undefined (skipped under strict:false)
        assert_eq!(key("'a' if count == 5"), None);
        assert_eq!(key("'a' if count == 3"), Some(json!("a")));
        // nested / right-associative — the bucket idiom
        assert_eq!(
            key("'high' if count > 5 else 'mid' if count > 1 else 'low'"),
            Some(json!("mid"))
        );
        // conditional nested inside parens and a filter arg
        assert_eq!(key("('a' if enabled else 'b') | upper"), Some(json!("A")));
        assert_eq!(
            key("nope | default('y' if enabled else 'n')"),
            Some(json!("y"))
        );
        // malformed: `if` without `else`-or-end mid-stream still parses (else optional),
        // but a dangling `else` with no condition is unsupported
        assert!(eval_key("'a' if", &vars()).is_err());
        assert!(eval_key("'a' if count == 3 else", &vars()).is_err());
    }

    #[test]
    fn ternary_filter() {
        // basic true/false selection
        assert_eq!(key("enabled | ternary('on', 'off')"), Some(json!("on")));
        assert_eq!(
            key("(count > 5) | ternary('big', 'small')"),
            Some(json!("small"))
        );
        // operand truthiness follows Jinja: non-empty string is truthy
        assert_eq!(key("tier | ternary('set', 'unset')"), Some(json!("set")));
        // undefined operand takes the false branch (bool(Undefined) is false), and crucially
        // does NOT error even though the var is missing
        assert_eq!(key("nope | ternary('yes', 'no')"), Some(json!("no")));
        // an undefined *untaken* branch is never evaluated, so it doesn't fail the host
        assert_eq!(key("enabled | ternary('yes', nope)"), Some(json!("yes")));
        // none operand with a none_val falls to the third branch; without one, false branch
        assert_eq!(
            key("missing | default(none) | ternary('t', 'f', 'n')"),
            Some(json!("n"))
        );
        assert_eq!(
            key("missing | default(none) | ternary('t', 'f')"),
            Some(json!("f"))
        );
        // nested ternary: the bucket idiom the parser used to reject
        assert_eq!(
            key("(count > 5) | ternary('high', (count > 1) | ternary('mid', 'low'))"),
            Some(json!("mid"))
        );
        // wrong arity is unsupported (fail-loud)
        assert!(eval_key("enabled | ternary('only')", &vars()).is_err());
        assert!(eval_key("enabled | ternary('a', 'b', 'c', 'd')", &vars()).is_err());
    }

    #[test]
    fn compose_coercion() {
        // A bare variable keeps its native type...
        assert_eq!(comp("count"), Some(json!(3)));
        // ...but any computed expression is re-typed like ansible's non-native Jinja:
        // numbers become strings, None becomes "", bools/lists pass through.
        assert_eq!(comp("count + 1"), Some(json!("4")));
        assert_eq!(comp("(count)"), Some(json!("3")));
        assert_eq!(comp("roles | length"), Some(json!("2")));
        assert_eq!(comp("count > 1"), Some(json!(true)));
        assert_eq!(comp("roles[::-1]"), Some(json!(["cache", "db"])));
        assert_eq!(comp("nothing | default(none)"), Some(json!("")));
    }

    #[test]
    fn compose_default_chains() {
        // region | default(zone[:-1]) where region is undefined
        let mut v = vars();
        v.remove("region");
        assert_eq!(
            eval_compose("region | default(zone[:-1])", &v, false).unwrap(),
            Some(json!("us-east-01"))
        );
        // zone | default(region + 'a') where zone is undefined
        let mut v = vars();
        v.remove("zone");
        assert_eq!(
            eval_compose("zone | default(region + 'a')", &v, false).unwrap(),
            Some(json!("us-east-01a"))
        );
    }

    #[test]
    fn compose_strict_skip_vs_error() {
        let mut v = vars();
        v.remove("region");
        v.remove("zone");
        // both undefined: default's fallback also errors -> skip under strict:false
        assert_eq!(
            eval_compose("region | default(zone[:-1])", &v, false).unwrap(),
            None
        );
        // type error swallowed under strict:false
        assert_eq!(
            eval_compose("region + count", &vars(), false).unwrap(),
            None
        );
        // ...but a strict run surfaces it as an error
        assert!(eval_compose("region + count", &vars(), true).is_err());
    }

    #[test]
    fn unsupported_expressions_error_loudly() {
        // unknown filter is unsupported even under strict:false compose
        assert!(eval_compose("name | madeupfilter", &vars(), false).is_err());
        assert!(eval_key("tier is match('go.*')", &vars()).is_err());
        assert!(eval_key("lockdown +", &vars()).is_err());
    }
}
