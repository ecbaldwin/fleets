//! INI inventory parser, mirroring ansible's `plugins/inventory/ini.py` dialect:
//! `[group]` host sections, `[group:children]`, `[group:vars]`, shlex-split host lines
//! with inline `key=value` vars, host ranges, and `ast.literal_eval` value coercion.

use std::collections::HashMap;
use std::path::Path;

use serde_json::Value;

use crate::error::{Error, Result};
use crate::hostrange::expand_hostpattern;
use crate::literal_eval::literal_eval;
use crate::model::{InventoryData, UNGROUPED};
use crate::sanitize::to_safe_group_name;

const COMMENT_MARKERS: &[char] = &['#', ';'];

#[derive(Clone, Copy, PartialEq)]
enum State {
    Hosts,
    Children,
    Vars,
}

struct Pending {
    state: State,
    parents: Vec<String>,
}

pub fn parse_ini_file(inv: &mut InventoryData, path: &Path) -> Result<()> {
    let text = std::fs::read_to_string(path).map_err(|e| Error::Io {
        path: path.display().to_string(),
        source: e,
    })?;
    parse_ini_str(inv, &text, path)
}

pub fn parse_ini_str(inv: &mut InventoryData, text: &str, path: &Path) -> Result<()> {
    let perr = |msg: String| Error::Parse {
        path: path.display().to_string(),
        msg,
    };

    // We behave as though the first line is '[ungrouped]'.
    let mut groupname = UNGROUPED.to_string();
    let mut state = State::Hosts;
    let mut pending: HashMap<String, Pending> = HashMap::new();

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty()
            || line.starts_with(COMMENT_MARKERS[0])
            || line.starts_with(COMMENT_MARKERS[1])
        {
            continue;
        }

        if let Some((g, tag)) = parse_section(line) {
            groupname = to_safe_group_name(&g);
            state = match tag.as_deref() {
                None | Some("hosts") => State::Hosts,
                Some("children") => State::Children,
                Some("vars") => State::Vars,
                Some(other) => return Err(perr(format!("Section [{g}:{other}] has unknown type"))),
            };

            if !inv.groups.contains_key(&groupname) {
                if state == State::Vars && !pending.contains_key(&groupname) {
                    pending.insert(
                        groupname.clone(),
                        Pending {
                            state: State::Vars,
                            parents: vec![],
                        },
                    );
                }
                inv.add_group(&groupname);
            }
            if state != State::Vars
                && pending
                    .get(&groupname)
                    .is_some_and(|p| p.state == State::Children)
            {
                add_pending_children(inv, &groupname, &mut pending);
            } else if state != State::Vars
                && pending
                    .get(&groupname)
                    .is_some_and(|p| p.state == State::Vars)
            {
                pending.remove(&groupname);
            }
            continue;
        } else if line.starts_with('[') && line.ends_with(']') {
            return Err(perr(format!("Invalid section entry: '{line}'")));
        }

        match state {
            State::Hosts => {
                let (hosts, port, vars) = parse_host_definition(line).map_err(perr)?;
                for h in hosts {
                    inv.add_host(&h);
                    inv.add_host_to_group(&h, &groupname);
                    if let Some(p) = port {
                        inv.set_host_var(&h, "ansible_port", Value::from(p));
                    }
                    for (k, v) in &vars {
                        inv.set_host_var(&h, k, v.clone());
                    }
                }
            }
            State::Vars => {
                let (k, v) = parse_variable_definition(line).map_err(perr)?;
                inv.set_group_var(&groupname, &k, v);
            }
            State::Children => {
                let child = to_safe_group_name(parse_group_name(line).trim());
                if inv.groups.contains_key(&child) {
                    inv.add_child(&groupname, &child);
                } else {
                    pending
                        .entry(child)
                        .or_insert_with(|| Pending {
                            state: State::Children,
                            parents: vec![],
                        })
                        .parents
                        .push(groupname.clone());
                }
            }
        }
    }
    Ok(())
}

/// Resolve a child group once it has been declared, linking it to every parent that
/// referenced it (and recursively resolving parents pending as children).
fn add_pending_children(
    inv: &mut InventoryData,
    group: &str,
    pending: &mut HashMap<String, Pending>,
) {
    let Some(p) = pending.remove(group) else {
        return;
    };
    inv.add_group(group);
    for parent in p.parents {
        inv.add_group(&parent);
        inv.add_child(&parent, group);
        if pending
            .get(&parent)
            .is_some_and(|pp| pp.state == State::Children)
        {
            add_pending_children(inv, &parent, pending);
        }
    }
}

/// Match `[group]` or `[group:tag]`, ignoring trailing whitespace/comment. Returns
/// `(group, Some(tag))` or `(group, None)`; `None` overall if not a section header.
fn parse_section(line: &str) -> Option<(String, Option<String>)> {
    if !line.starts_with('[') {
        return None;
    }
    let close = line.find(']')?;
    let inner = &line[1..close];
    // Trailing content after ']' must be empty or a comment.
    let rest = line[close + 1..].trim_start();
    if !rest.is_empty() && !rest.starts_with('#') {
        return None;
    }
    // group name: chars excluding ':', ']', whitespace.
    if inner.is_empty() || inner.contains(char::is_whitespace) {
        return None;
    }
    match inner.split_once(':') {
        Some((g, tag))
            if !g.is_empty()
                && !tag.is_empty()
                && tag.chars().all(|c| c.is_alphanumeric() || c == '_') =>
        {
            Some((g.to_string(), Some(tag.to_string())))
        }
        Some(_) => None,
        None => Some((inner.to_string(), None)),
    }
}

fn parse_group_name(line: &str) -> &str {
    // Strip an inline comment, then take the first whitespace-delimited token.
    let line = line.split('#').next().unwrap_or(line).trim();
    line.split_whitespace().next().unwrap_or(line)
}

/// Expanded hostnames, an optional shared port, and inline host vars.
type HostDef = (Vec<String>, Option<u32>, Vec<(String, Value)>);

/// Parse a host definition line: a host pattern (with optional port) followed by inline
/// `key=value` assignments. Mirrors `_parse_host_definition`.
fn parse_host_definition(line: &str) -> std::result::Result<HostDef, String> {
    let tokens = shlex_split(line);
    if tokens.is_empty() {
        return Err(format!("Empty host definition: '{line}'"));
    }
    let (hosts, port) = expand_hostpattern(&tokens[0]).map_err(|e| e.to_string())?;
    let mut vars = Vec::new();
    for t in &tokens[1..] {
        let (k, v) = t
            .split_once('=')
            .ok_or_else(|| format!("Expected key=value host variable assignment, got: {t}"))?;
        vars.push((k.to_string(), parse_value(v)));
    }
    Ok((hosts, port, vars))
}

/// Parse a `[group:vars]` line `key = value`.
fn parse_variable_definition(line: &str) -> std::result::Result<(String, Value), String> {
    let (k, v) = line
        .split_once('=')
        .ok_or_else(|| format!("Expected key=value, got: {line}"))?;
    Ok((k.trim().to_string(), parse_value(v.trim())))
}

/// Coerce an INI value: try a Python literal, else keep the raw string (`_parse_value`).
fn parse_value(v: &str) -> Value {
    literal_eval(v).unwrap_or_else(|| Value::String(v.to_string()))
}

/// A minimal POSIX-style `shlex.split(comments=True)`: whitespace-delimited tokens with
/// single/double quoting, backslash escapes, and `#` starting a comment at a token boundary.
fn shlex_split(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut in_token = false;
    let mut chars = line.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '#' if !in_token => break, // comment to end of line (at a token boundary)
            c if c.is_whitespace() => {
                if in_token {
                    tokens.push(std::mem::take(&mut cur));
                    in_token = false;
                }
            }
            '\'' => {
                in_token = true;
                for q in chars.by_ref() {
                    if q == '\'' {
                        break;
                    }
                    cur.push(q);
                }
            }
            '"' => {
                in_token = true;
                while let Some(q) = chars.next() {
                    match q {
                        '"' => break,
                        '\\' => {
                            if let Some(&e) = chars.peek() {
                                chars.next();
                                cur.push(e);
                            }
                        }
                        other => cur.push(other),
                    }
                }
            }
            '\\' => {
                in_token = true;
                if let Some(e) = chars.next() {
                    cur.push(e);
                }
            }
            other => {
                in_token = true;
                cur.push(other);
            }
        }
    }
    if in_token {
        tokens.push(cur);
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shlex_basic() {
        assert_eq!(
            shlex_split("beta:2345 user=admin # c"),
            ["beta:2345", "user=admin"]
        );
        assert_eq!(shlex_split("h var=\"some value\""), ["h", "var=some value"]);
    }

    #[test]
    fn section_parsing() {
        assert_eq!(parse_section("[web]"), Some(("web".into(), None)));
        assert_eq!(
            parse_section("[web:children]"),
            Some(("web".into(), Some("children".into())))
        );
        assert_eq!(
            parse_section("[web:vars] # x"),
            Some(("web".into(), Some("vars".into())))
        );
        assert_eq!(parse_section("not a section"), None);
    }
}
