//! Group-name sanitization, mirroring ansible's `to_safe_group_name`
//! (`inventory/group.py`) with `TRANSFORM_INVALID_GROUP_CHARS=silently`.
//!
//! Ansible applies the regex `^[\d\W]|[^\w]` and replaces every match with `_`:
//!   * a leading digit or leading non-word character, and
//!   * any non-word character anywhere.
//!
//! A "word" character is `[A-Za-z0-9_]`. We treat word-ness as ASCII; ansible uses
//! Python's Unicode-aware `\w`, but inventory names are ASCII in practice.

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Convert a raw group name to its ansible-safe form. Empty names pass through.
pub fn to_safe_group_name(name: &str) -> String {
    if name.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(name.len());
    for (i, c) in name.chars().enumerate() {
        let invalid = if i == 0 {
            // `^[\d\W]`: leading digit or leading non-word char.
            c.is_ascii_digit() || !is_word(c)
        } else {
            // `[^\w]`: any non-word char.
            !is_word(c)
        };
        out.push(if invalid { '_' } else { c });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::to_safe_group_name as s;

    #[test]
    fn replaces_dashes_and_dots() {
        assert_eq!(s("web-servers"), "web_servers");
        assert_eq!(s("web.east"), "web_east");
    }

    #[test]
    fn replaces_leading_digit_only() {
        assert_eq!(s("1web"), "_web");
        assert_eq!(s("web1"), "web1");
        assert_eq!(s("w1b"), "w1b");
    }

    #[test]
    fn keeps_clean_names() {
        assert_eq!(s("all"), "all");
        assert_eq!(s("web_servers_2"), "web_servers_2");
    }

    #[test]
    fn empty_passthrough() {
        assert_eq!(s(""), "");
    }
}
