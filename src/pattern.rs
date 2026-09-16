//! Host pattern matching for `-l/--limit`, mirroring ansible's `InventoryManager`
//! pattern engine (`split_host_pattern`, `order_patterns`, `_evaluate_patterns`,
//! `_match_one_pattern`, `_enumerate_matches`).
//!
//! Pattern kinds: a plain name (matches itself), a shell glob (`*`/`?`/`[seq]`/`[!seq]`),
//! or a `~`-prefixed regex. Patterns combine with `:`/`,` (union), `&` (intersection), and
//! `!` (exclusion); regular patterns apply first, then intersections, then exclusions.

use std::collections::HashSet;

use regex::Regex;

use crate::model::InventoryData;

/// Compute the set of host names selected by a `--limit` expression.
pub fn select_hosts(inv: &InventoryData, limit: &str) -> HashSet<String> {
    evaluate_patterns(inv, &split_host_pattern(limit))
        .into_iter()
        .collect()
}

/// Split a limit string into individual patterns. Commas always separate; otherwise `:`
/// separates, but a complete `[...]` group (a host range) is kept intact.
pub fn split_host_pattern(pattern: &str) -> Vec<String> {
    let raw: Vec<String> = if pattern.contains(',') {
        pattern.split(',').map(str::to_string).collect()
    } else {
        // Walk the string, splitting on out-of-bracket ':'.
        let mut parts = Vec::new();
        let mut cur = String::new();
        let mut depth = 0i32;
        for c in pattern.chars() {
            match c {
                '[' => {
                    depth += 1;
                    cur.push(c);
                }
                ']' => {
                    depth -= 1;
                    cur.push(c);
                }
                ':' if depth == 0 => {
                    parts.push(std::mem::take(&mut cur));
                }
                _ => cur.push(c),
            }
        }
        parts.push(cur);
        parts
    };
    raw.into_iter()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

/// Reorder patterns: regular first, then `&` intersections, then `!` exclusions. If no
/// regular pattern is present, `all` is implied so `&`/`!` have something to operate on.
fn order_patterns(patterns: &[String]) -> Vec<String> {
    let (mut regular, mut intersection, mut exclude) = (Vec::new(), Vec::new(), Vec::new());
    for p in patterns {
        if p.is_empty() {
            continue;
        }
        match p.as_bytes()[0] {
            b'!' => exclude.push(p.clone()),
            b'&' => intersection.push(p.clone()),
            _ => regular.push(p.clone()),
        }
    }
    if regular.is_empty() {
        regular.push("all".to_string());
    }
    regular.extend(intersection);
    regular.extend(exclude);
    regular
}

/// Evaluate ordered patterns into a list of host names, applying union/intersection/
/// exclusion. Mirrors `_evaluate_patterns`.
fn evaluate_patterns(inv: &InventoryData, patterns: &[String]) -> Vec<String> {
    let mut hosts: Vec<String> = Vec::new();
    for p in order_patterns(patterns) {
        // A pattern that is exactly a host name shortcuts to that host.
        if inv.hosts.contains_key(&p) {
            if !hosts.contains(&p) {
                hosts.push(p);
            }
            continue;
        }
        let matched = match_one_pattern(inv, &p);
        match p.as_bytes()[0] {
            b'!' => {
                let drop: HashSet<&String> = matched.iter().collect();
                hosts.retain(|h| !drop.contains(h));
            }
            b'&' => {
                let keep: HashSet<&String> = matched.iter().collect();
                hosts.retain(|h| keep.contains(h));
            }
            _ => {
                let existing: HashSet<String> = hosts.iter().cloned().collect();
                for h in matched {
                    if !existing.contains(&h) {
                        hosts.push(h);
                    }
                }
            }
        }
    }
    hosts
}

/// Match a single pattern (the leading `&`/`!` is stripped). Mirrors `_match_one_pattern`
/// and `_enumerate_matches`: matching group names contribute all their (recursive) hosts,
/// and glob/regex patterns also match host names directly.
fn match_one_pattern(inv: &InventoryData, pattern: &str) -> Vec<String> {
    let pattern = match pattern.as_bytes().first() {
        Some(b'&') | Some(b'!') => &pattern[1..],
        _ => pattern,
    };

    let re = match compile(pattern) {
        Some(re) => re,
        None => return Vec::new(),
    };

    let mut results: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    // Groups whose name matches contribute all hosts in the group and its descendants.
    let group_names: Vec<String> = inv.groups.keys().cloned().collect();
    let mut matched_group = false;
    for g in &group_names {
        if re.is_match(g) {
            matched_group = true;
            for h in inv.group_all_hosts(g) {
                if seen.insert(h.clone()) {
                    results.push(h);
                }
            }
        }
    }

    // Match host names too when no group matched, or the pattern is a regex/glob.
    let is_glob = pattern.starts_with('~') || pattern.contains(['.', '?', '*', '[']);
    if !matched_group || is_glob {
        for h in inv.hosts.keys() {
            if re.is_match(h) && seen.insert(h.clone()) {
                results.push(h.clone());
            }
        }
    }
    results
}

/// Compile a pattern into a `Regex`. A `~` prefix is a raw (start-anchored) regex;
/// otherwise it is a shell glob translated to a fully-anchored regex.
fn compile(pattern: &str) -> Option<Regex> {
    if let Some(rest) = pattern.strip_prefix('~') {
        // Python `re.match`: anchored at start only.
        Regex::new(&format!("(?s)^(?:{rest})")).ok()
    } else {
        Regex::new(&glob_to_regex(pattern)).ok()
    }
}

/// Translate a shell glob to a fully-anchored regex, matching `fnmatch.translate`:
/// `*`→`.*`, `?`→`.`, `[seq]`/`[!seq]` char classes, everything else literal.
fn glob_to_regex(glob: &str) -> String {
    let mut out = String::from("(?s)^");
    let chars: Vec<char> = glob.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '*' => out.push_str(".*"),
            '?' => out.push('.'),
            '[' => {
                // Find the matching ']'. A ']' right after '[' or '[!' is a literal member.
                let mut j = i + 1;
                if j < chars.len() && (chars[j] == '!' || chars[j] == '^') {
                    j += 1;
                }
                if j < chars.len() && chars[j] == ']' {
                    j += 1;
                }
                while j < chars.len() && chars[j] != ']' {
                    j += 1;
                }
                if j >= chars.len() {
                    // No closing bracket: treat '[' as a literal.
                    out.push_str("\\[");
                } else {
                    let mut class: String = chars[i + 1..j].iter().collect();
                    if let Some(stripped) = class.strip_prefix('!') {
                        class = format!("^{stripped}");
                    }
                    out.push('[');
                    out.push_str(&class);
                    out.push(']');
                    i = j + 1;
                    continue;
                }
            }
            other => out.push_str(&regex::escape(&other.to_string())),
        }
        i += 1;
    }
    out.push('$');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_colon_and_comma() {
        assert_eq!(split_host_pattern("a:b:c"), ["a", "b", "c"]);
        assert_eq!(split_host_pattern("a, b ,c"), ["a", "b", "c"]);
        assert_eq!(
            split_host_pattern("web*:&staged:!excluded"),
            ["web*", "&staged", "!excluded"]
        );
        // A bracketed range is kept intact.
        assert_eq!(split_host_pattern("web[01:03]"), ["web[01:03]"]);
    }

    #[test]
    fn glob_translation() {
        assert_eq!(glob_to_regex("web*"), "(?s)^web.*$");
        assert_eq!(glob_to_regex("h?"), "(?s)^h.$");
        assert_eq!(glob_to_regex("h[!0]"), "(?s)^h[^0]$");
    }
}
