//! A small emulation of Python's `ast.literal_eval`, used for INI value coercion
//! (`ini.py:_parse_value`). On any parse failure ansible keeps the original string, so
//! [`literal_eval`] returns `None` and the caller falls back to the raw string.
//!
//! Supports the literal grammar ansible relies on: `True`/`False`/`None`, ints, floats,
//! single/double-quoted strings, lists, tuples, and dicts. Python tuples have no JSON
//! equivalent, so they decode to arrays (matching how ansible's JSON output renders them).

use serde_json::{Map, Number, Value};

/// Parse `s` as a Python literal. Returns `None` if `s` is not a complete literal (the
/// caller then treats the original text as a plain string, like ansible).
pub fn literal_eval(s: &str) -> Option<Value> {
    let mut p = Parser {
        chars: s.chars().collect(),
        pos: 0,
    };
    p.skip_ws();
    let v = p.parse_value()?;
    p.skip_ws();
    if p.pos == p.chars.len() {
        Some(v)
    } else {
        None
    }
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(c) if c.is_whitespace()) {
            self.pos += 1;
        }
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn parse_value(&mut self) -> Option<Value> {
        self.skip_ws();
        match self.peek()? {
            '\'' | '"' => self.parse_string().map(Value::String),
            '[' => self.parse_seq('[', ']'),
            '(' => self.parse_seq('(', ')'),
            '{' => self.parse_dict(),
            _ => self.parse_atom(),
        }
    }

    fn parse_string(&mut self) -> Option<String> {
        let quote = self.peek()?;
        self.pos += 1;
        let mut out = String::new();
        loop {
            let c = self.peek()?;
            self.pos += 1;
            match c {
                '\\' => {
                    let e = self.peek()?;
                    self.pos += 1;
                    out.push(match e {
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        '\\' => '\\',
                        '\'' => '\'',
                        '"' => '"',
                        other => other,
                    });
                }
                c if c == quote => return Some(out),
                c => out.push(c),
            }
        }
    }

    /// Parse a list `[...]` or tuple `(...)`; both decode to a JSON array.
    fn parse_seq(&mut self, open: char, close: char) -> Option<Value> {
        self.eat(open).then_some(())?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.eat(close) {
            return Some(Value::Array(items));
        }
        loop {
            items.push(self.parse_value()?);
            self.skip_ws();
            if self.eat(close) {
                return Some(Value::Array(items));
            }
            if !self.eat(',') {
                return None;
            }
            self.skip_ws();
            // Allow a trailing comma before the close.
            if self.eat(close) {
                return Some(Value::Array(items));
            }
        }
    }

    fn parse_dict(&mut self) -> Option<Value> {
        self.eat('{').then_some(())?;
        let mut map = Map::new();
        self.skip_ws();
        if self.eat('}') {
            return Some(Value::Object(map));
        }
        loop {
            self.skip_ws();
            // Dict keys may be any literal; JSON requires string keys, so render the key
            // to a string (numbers/bools become their textual form, as Python str would).
            let key = self.parse_value()?;
            let key = match key {
                Value::String(s) => s,
                other => other.to_string(),
            };
            self.skip_ws();
            self.eat(':').then_some(())?;
            let val = self.parse_value()?;
            map.insert(key, val);
            self.skip_ws();
            if self.eat('}') {
                return Some(Value::Object(map));
            }
            if !self.eat(',') {
                return None;
            }
            self.skip_ws();
            if self.eat('}') {
                return Some(Value::Object(map));
            }
        }
    }

    /// Parse a bare token: `True`/`False`/`None` or a number.
    fn parse_atom(&mut self) -> Option<Value> {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_whitespace() || matches!(c, ',' | ']' | '}' | ')' | ':') {
                break;
            }
            self.pos += 1;
        }
        let tok: String = self.chars[start..self.pos].iter().collect();
        match tok.as_str() {
            "True" => Some(Value::Bool(true)),
            "False" => Some(Value::Bool(false)),
            "None" => Some(Value::Null),
            _ => parse_number(&tok),
        }
    }
}

/// Parse a token as a Python numeric literal, honoring its rules (no leading zeros for
/// decimal integers; `1.0` is a float). Returns `None` for anything else.
fn parse_number(tok: &str) -> Option<Value> {
    if tok.is_empty() {
        return None;
    }
    let body = tok.strip_prefix(['+', '-']).unwrap_or(tok);

    // Integer: all digits, and no leading zero unless the value is exactly "0".
    if !body.is_empty() && body.bytes().all(|b| b.is_ascii_digit()) {
        if body.len() > 1 && body.starts_with('0') {
            return None; // Python: leading zeros are a SyntaxError -> not a literal.
        }
        return tok.parse::<i64>().ok().map(|n| Value::Number(n.into()));
    }

    // Float: must contain a '.' or exponent and parse as f64. Reject bare "." etc.
    if body.contains('.') || body.contains(['e', 'E']) {
        if let Ok(f) = tok.parse::<f64>() {
            return Number::from_f64(f).map(Value::Number);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::literal_eval as le;
    use serde_json::json;

    #[test]
    fn scalars() {
        assert_eq!(le("1"), Some(json!(1)));
        assert_eq!(le("1.0"), Some(json!(1.0)));
        assert_eq!(le("True"), Some(json!(true)));
        assert_eq!(le("False"), Some(json!(false)));
        assert_eq!(le("None"), Some(json!(null)));
        assert_eq!(le("-5"), Some(json!(-5)));
    }

    #[test]
    fn strings_and_fallbacks() {
        assert_eq!(le("'hello'"), Some(json!("hello")));
        assert_eq!(le("\"hi\""), Some(json!("hi")));
        // Not literals -> None (caller keeps the raw string).
        assert_eq!(le("foo"), None);
        assert_eq!(le("01"), None); // leading zero -> string "01"
        assert_eq!(le("8.8.8.8"), None);
    }

    #[test]
    fn collections() {
        assert_eq!(le("[1, 2, 3]"), Some(json!([1, 2, 3])));
        assert_eq!(le("(1, 'a')"), Some(json!([1, "a"])));
        assert_eq!(
            le("{'a': 1, 'b': [2, 3]}"),
            Some(json!({"a": 1, "b": [2, 3]}))
        );
        assert_eq!(le("[]"), Some(json!([])));
    }
}
