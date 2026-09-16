//! Host pattern parsing: `host:port` splitting and `[beg:end:step]` range expansion,
//! porting ansible's `expand_hostname_range` / `_expand_hostpattern`
//! (`plugins/inventory/__init__.py`).

use crate::error::{Error, Result};

const ASCII_LETTERS: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";

/// Does this pattern contain a range? (ansible's `detect_range` is just "has a `[`".)
fn detect_range(s: &str) -> bool {
    s.contains('[')
}

/// Split an optional trailing `:port` off a host pattern, ignoring colons inside `[...]`
/// ranges. Returns `(pattern, Some(port))` when the final out-of-bracket colon is followed
/// by digits, else `(pattern, None)`.
pub fn split_port(pattern: &str) -> (String, Option<u32>) {
    let mut depth = 0i32;
    let mut last_colon = None;
    for (i, c) in pattern.char_indices() {
        match c {
            '[' => depth += 1,
            ']' => depth -= 1,
            ':' if depth == 0 => last_colon = Some(i),
            _ => {}
        }
    }
    if let Some(i) = last_colon {
        let (head, tail) = (&pattern[..i], &pattern[i + 1..]);
        if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) {
            if let Ok(port) = tail.parse::<u32>() {
                return (head.to_string(), Some(port));
            }
        }
    }
    (pattern.to_string(), None)
}

/// Expand a host pattern into concrete hostnames and an optional shared port.
pub fn expand_hostpattern(pattern: &str) -> Result<(Vec<String>, Option<u32>)> {
    let (pattern, port) = split_port(pattern);
    let hosts = if detect_range(&pattern) {
        expand_hostname_range(&pattern)?
    } else {
        vec![pattern]
    };
    Ok((hosts, port))
}

/// Expand a single `[beg:end:step]` range (recursively handling multiple ranges in one
/// pattern), porting ansible's `expand_hostname_range`.
pub fn expand_hostname_range(line: &str) -> Result<Vec<String>> {
    // Replace only the first '[' and ']' with '|' to split head/range/tail.
    let replaced = line.replacen('[', "|", 1).replacen(']', "|", 1);
    let parts: Vec<&str> = replaced.splitn(3, '|').collect();
    if parts.len() != 3 {
        return Err(range_err("host range is malformed"));
    }
    let (head, nrange, tail) = (parts[0], parts[1], parts[2]);

    let bounds: Vec<&str> = nrange.split(':').collect();
    if bounds.len() != 2 && bounds.len() != 3 {
        return Err(range_err("host range must be begin:end or begin:end:step"));
    }
    let mut beg = bounds[0].to_string();
    let end = bounds[1];
    let step_str = if bounds.len() == 3 { bounds[2] } else { "1" };
    if beg.is_empty() {
        beg = "0".to_string();
    }
    if end.is_empty() {
        return Err(range_err("host range must specify end value"));
    }
    let step: i64 = step_str
        .parse()
        .map_err(|_| range_err("invalid step in host range"))?;
    if step == 0 {
        return Err(range_err("host range step cannot be zero"));
    }

    // Zero-padding hint: if begin starts with '0' and is longer than one char.
    let zfill = if beg.starts_with('0') && beg.len() > 1 {
        if beg.len() != end.len() {
            return Err(range_err(
                "host range must specify equal-length begin and end formats",
            ));
        }
        Some(beg.len())
    } else {
        None
    };

    // Alphabetic range if both ends are single ascii letters.
    let seq: Vec<String> = match (ASCII_LETTERS.find(&beg), ASCII_LETTERS.find(end)) {
        (Some(i_beg), Some(i_end)) if beg.len() == 1 && end.len() == 1 => {
            if i_beg > i_end {
                return Err(range_err("host range must have begin <= end"));
            }
            ASCII_LETTERS
                .chars()
                .skip(i_beg)
                .take(i_end - i_beg + 1)
                .step_by(step as usize)
                .map(|c| c.to_string())
                .collect()
        }
        _ => {
            // Numeric range.
            let b: i64 = beg
                .parse()
                .map_err(|_| range_err("invalid begin in host range"))?;
            let e: i64 = end
                .parse()
                .map_err(|_| range_err("invalid end in host range"))?;
            let mut out = Vec::new();
            let mut i = b;
            while i <= e {
                out.push(match zfill {
                    Some(w) => format!("{i:0width$}", width = w),
                    None => i.to_string(),
                });
                i += step;
            }
            out
        }
    };

    let mut all_hosts = Vec::new();
    for token in seq {
        let hname = format!("{head}{token}{tail}");
        if detect_range(&hname) {
            all_hosts.extend(expand_hostname_range(&hname)?);
        } else {
            all_hosts.push(hname);
        }
    }
    Ok(all_hosts)
}

fn range_err(msg: &str) -> Error {
    Error::Parse {
        path: "<host range>".into(),
        msg: msg.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_zero_padded() {
        let (h, _) = expand_hostpattern("web[01:03].x").unwrap();
        assert_eq!(h, ["web01.x", "web02.x", "web03.x"]);
    }

    #[test]
    fn step_and_unpadded() {
        assert_eq!(
            expand_hostname_range("n[1:5:2]").unwrap(),
            ["n1", "n3", "n5"]
        );
    }

    #[test]
    fn alpha_range() {
        assert_eq!(expand_hostname_range("h[a:c]").unwrap(), ["ha", "hb", "hc"]);
    }

    #[test]
    fn port_split() {
        assert_eq!(split_port("web01:2222"), ("web01".into(), Some(2222)));
        assert_eq!(split_port("web[01:03].x"), ("web[01:03].x".into(), None));
        assert_eq!(split_port("web[01:03]:22"), ("web[01:03]".into(), Some(22)));
    }
}
