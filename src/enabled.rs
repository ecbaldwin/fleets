//! Which inventory source *kinds* are enabled, mirroring ansible's `INVENTORY_ENABLED`
//! (`ANSIBLE_INVENTORY_ENABLED`).
//!
//! ansible tries a list of inventory *plugins* in order; the first whose `verify_file`
//! accepts a source claims it. fleets dispatches by file extension / executability instead,
//! so we don't reproduce strict ordering (see `docs/environment.md`); we honor the *set* of
//! enabled kinds. The plugin → kind mapping:
//!
//! | ansible plugin | fleets kind        |
//! |----------------|--------------------|
//! | `yaml`         | `.yaml/.yml/.json` + extensionless structured |
//! | `ini`          | `.ini`             |
//! | `toml`         | `.toml`            |
//! | `script`       | executable, no inventory extension |
//! | `auto`         | `plugin:`-key dispatch (gates `constructed`) |
//! | `host_list`    | inline `-i h1,h2,` — unsupported, ignored |

/// The set of enabled source kinds. All-true by default (matching ansible's default
/// `host_list, script, auto, yaml, ini, toml`, restricted to the kinds fleets supports).
#[derive(Debug, Clone, Copy)]
pub struct EnabledKinds {
    pub yaml: bool,
    pub ini: bool,
    pub toml: bool,
    pub script: bool,
    pub auto: bool,
}

impl Default for EnabledKinds {
    fn default() -> Self {
        EnabledKinds {
            yaml: true,
            ini: true,
            toml: true,
            script: true,
            auto: true,
        }
    }
}

impl EnabledKinds {
    /// Resolve from the process environment. If `ANSIBLE_INVENTORY_ENABLED` is set it
    /// *replaces* the default list entirely (as in ansible): only the named plugins are
    /// enabled. Unset → all kinds enabled.
    pub fn from_env() -> Self {
        match std::env::var("ANSIBLE_INVENTORY_ENABLED") {
            Ok(v) => Self::parse(&v),
            Err(_) => Self::default(),
        }
    }

    /// Parse a comma-separated plugin list into an enabled set. Names fleets doesn't model
    /// (`host_list`, `advanced_host_list`, `nmap`, …) are accepted but have no effect.
    pub fn parse(list: &str) -> Self {
        let mut k = EnabledKinds {
            yaml: false,
            ini: false,
            toml: false,
            script: false,
            auto: false,
        };
        for name in list.split(',') {
            match name.trim() {
                "yaml" => k.yaml = true,
                "ini" => k.ini = true,
                "toml" => k.toml = true,
                "script" => k.script = true,
                "auto" => k.auto = true,
                _ => {} // host_list and unsupported/unknown plugins: no-op
            }
        }
        k
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_enables_every_supported_kind() {
        let k = EnabledKinds::default();
        assert!(k.yaml && k.ini && k.toml && k.script && k.auto);
    }

    #[test]
    fn parse_enables_only_listed_kinds() {
        let k = EnabledKinds::parse("yaml,ini");
        assert!(k.yaml && k.ini);
        assert!(!k.toml && !k.script && !k.auto);
    }

    #[test]
    fn parse_is_order_and_whitespace_insensitive() {
        let k = EnabledKinds::parse("  auto , script ");
        assert!(k.auto && k.script);
        assert!(!k.yaml && !k.ini && !k.toml);
    }

    #[test]
    fn parse_ignores_unknown_and_unsupported_plugins() {
        // ansible's default list, plus plugins fleets doesn't model.
        let k = EnabledKinds::parse("host_list,script,auto,yaml,ini,toml,nmap,advanced_host_list");
        assert!(k.yaml && k.ini && k.toml && k.script && k.auto);
    }

    #[test]
    fn parse_empty_disables_everything() {
        let k = EnabledKinds::parse("");
        assert!(!k.yaml && !k.ini && !k.toml && !k.script && !k.auto);
    }
}
