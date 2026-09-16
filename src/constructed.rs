//! The `ansible.builtin.constructed` inventory plugin (minimal subset).
//!
//! `constructed` is a *post-processing* plugin: it runs at the position its source sorts to
//! and synthesizes groups from the host vars known *so far* (see `docs/anomalies.md`). We
//! implement all three features — `compose` (new host vars), `keyed_groups`, and `groups` —
//! driven by the small expression evaluator in [`crate::expr`]. The expression grammar is
//! intentionally narrow and errors loudly on syntax outside it rather than guessing.
//!
//! Ordering matches ansible: per host, `compose` runs first and its results are written as
//! host vars, then `groups` and `keyed_groups` run against the post-compose view. The
//! `compose` expressions themselves are all evaluated against the *pre-compose* snapshot —
//! they do not observe each other's results (verified differentially; see §32).
//!
//! Variable visibility matches ansible: only inventory **inline** vars are seen, not
//! `group_vars/`/`host_vars/` files (those load after parsing). We use
//! [`crate::vars::resolve_host_inline_vars`] accordingly.

use std::path::Path;

use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::Value;

use crate::error::{Error, Result};
use crate::expr;
use crate::model::InventoryData;

fn default_true() -> bool {
    true
}

/// Deserialize a `compose`/`groups` map, coercing non-string scalar values to the string
/// form ansible would produce. ansible builds these expressions by `%s`-formatting the raw
/// YAML value into a Jinja template (`"{%% if %s %%}..."` for `groups`, `"{{%s}}"` for
/// `compose`), so a value YAML parsed as a bool/number/null is accepted and stringified
/// Python-style (`true` -> "True", `1` -> "1", `null` -> "None") rather than hard-erroring.
fn de_expr_map<'de, D>(deserializer: D) -> std::result::Result<IndexMap<String, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw: IndexMap<String, Value> = IndexMap::deserialize(deserializer)?;
    raw.into_iter()
        .map(|(k, v)| {
            value_to_expr(&v)
                .map(|s| (k, s))
                .map_err(serde::de::Error::custom)
        })
        .collect()
}

/// Mirror Python's `%s` stringification of a scalar YAML value into a Jinja expression.
fn value_to_expr(v: &Value) -> std::result::Result<String, String> {
    match v {
        Value::String(s) => Ok(s.clone()),
        Value::Bool(b) => Ok(if *b { "True".into() } else { "False".into() }),
        Value::Number(n) => Ok(n.to_string()),
        Value::Null => Ok("None".into()),
        Value::Array(_) | Value::Object(_) => {
            Err(format!("expected a string expression, found {v}"))
        }
    }
}
fn default_separator() -> String {
    "_".to_string()
}

#[derive(Debug, Deserialize)]
struct Config {
    /// `compose` entries: new host var name -> Jinja expression. Evaluated first, against
    /// the pre-compose host-var snapshot, and written back as host vars.
    #[serde(default, deserialize_with = "de_expr_map")]
    compose: IndexMap<String, String>,
    #[serde(default)]
    keyed_groups: Vec<KeyedGroup>,
    #[serde(default, deserialize_with = "de_expr_map")]
    groups: IndexMap<String, String>,
    /// ansible's `strict` option (default false): when false, a `compose` expression that
    /// fails to evaluate (undefined var, type error) is silently skipped; when true it is
    /// a hard error.
    #[serde(default)]
    strict: bool,
    /// When the prefix is empty, whether a leading separator is still emitted. Default true.
    #[serde(default = "default_true")]
    leading_separator: bool,
    /// Whether a falsey-but-defined key with no `default_value` still makes a `prefix+sep`
    /// group. Default true.
    #[serde(default = "default_true")]
    trailing_separator: bool,
}

#[derive(Debug, Deserialize)]
struct KeyedGroup {
    key: String,
    #[serde(default)]
    prefix: String,
    #[serde(default = "default_separator")]
    separator: String,
    #[serde(default)]
    default_value: Option<String>,
    #[serde(default)]
    parent_group: Option<String>,
}

/// True if a decoded inventory document is a `constructed` plugin config.
pub fn is_constructed(value: &Value) -> bool {
    matches!(
        value.get("plugin").and_then(|p| p.as_str()),
        Some("constructed") | Some("ansible.builtin.constructed")
    )
}

/// Run the constructed pass against the inventory built so far.
pub fn apply(inv: &mut InventoryData, value: &Value, path: &Path) -> Result<()> {
    let cfg: Config = serde_json::from_value(value.clone()).map_err(|e| Error::Parse {
        path: path.display().to_string(),
        msg: format!("invalid constructed config: {e}"),
    })?;

    let err = |msg: String| Error::Parse {
        path: path.display().to_string(),
        msg,
    };

    // Snapshot host names: the pass adds groups/memberships, never hosts, so the set of
    // hosts to process is fixed up front (and avoids borrowing `inv` while mutating it).
    let hosts: Vec<String> = inv.hosts.keys().cloned().collect();

    let _span = tracing::info_span!(
        "constructed",
        hosts = hosts.len(),
        compose = cfg.compose.len(),
        keyed_groups = cfg.keyed_groups.len(),
        groups = cfg.groups.len(),
    )
    .entered();

    for host in &hosts {
        // `compose`: evaluate every entry against the same pre-compose snapshot (entries do
        // not see each other), then write the results back as host vars. Done before
        // `groups`/`keyed_groups` so those observe the composed vars.
        if !cfg.compose.is_empty() {
            let snapshot = crate::vars::resolve_host_inline_vars(inv, host);
            let mut composed: Vec<(String, Value)> = Vec::new();
            for (name, src) in &cfg.compose {
                match expr::eval_compose(src, &snapshot, cfg.strict) {
                    Ok(Some(v)) => composed.push((name.clone(), v)),
                    Ok(None) => {} // strict:false skip
                    Err(e) => return Err(err(format!("constructed compose[{name}]: {}", e.0))),
                }
            }
            for (name, v) in composed {
                inv.set_host_var(host, &name, v);
            }
        }

        let vars = crate::vars::resolve_host_inline_vars(inv, host);

        // `groups`: add the host to each group whose condition is truthy.
        for (gname, cond) in &cfg.groups {
            match expr::eval_condition(cond, &vars) {
                Ok(true) => {
                    let g = inv.add_group(gname);
                    inv.add_host_to_group(host, &g);
                }
                Ok(false) => {}
                Err(e) => return Err(err(format!("constructed groups[{gname}]: {}", e.0))),
            }
        }

        // `keyed_groups`: bucket the host by each key's value.
        for kg in &cfg.keyed_groups {
            let bares = match expr::eval_key(&kg.key, &vars) {
                Ok(None) => continue, // undefined key -> skip this host for this key
                Ok(Some(v)) => bare_names(&v, kg, &cfg)
                    .map_err(|m| err(format!("constructed keyed_groups key {:?}: {m}", kg.key)))?,
                Err(e) => return Err(err(format!("constructed keyed_groups: {}", e.0))),
            };
            for bare in bares {
                let raw = group_name(&kg.prefix, &kg.separator, &bare, cfg.leading_separator);
                let g = inv.add_group(&raw);
                inv.add_host_to_group(host, &g);
                if let Some(pg) = &kg.parent_group {
                    let parent = inv.add_group(pg);
                    inv.add_child(&parent, &g);
                }
            }
        }
    }
    Ok(())
}

/// The bare group-name component(s) a key value contributes, before prefix/separator.
fn bare_names(
    v: &Value,
    kg: &KeyedGroup,
    cfg: &Config,
) -> std::result::Result<Vec<String>, String> {
    if expr::truthy(v) {
        match v {
            Value::String(s) => Ok(vec![s.clone()]),
            Value::Number(n) => Ok(vec![n.to_string()]),
            Value::Bool(b) => Ok(vec![if *b { "True".into() } else { "False".into() }]),
            Value::Array(items) => {
                let mut out = Vec::new();
                for item in items {
                    out.push(scalar_to_string(item)?);
                }
                Ok(out)
            }
            // Mapping keys are a valid ansible feature we haven't implemented yet.
            Value::Object(_) => Err("dict-valued keys are not supported".into()),
            Value::Null => Ok(vec![]),
        }
    } else {
        // Defined but falsey: use default_value, else a trailing-separator-only group.
        match &kg.default_value {
            Some(dv) => Ok(vec![dv.clone()]),
            None if cfg.trailing_separator => Ok(vec![String::new()]),
            None => Ok(vec![]),
        }
    }
}

fn scalar_to_string(v: &Value) -> std::result::Result<String, String> {
    match v {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Bool(b) => Ok(if *b { "True".into() } else { "False".into() }),
        _ => Err("list elements must be scalars".into()),
    }
}

/// Build `prefix + separator + bare`, then sanitize. Mirrors ansible: the separator is
/// dropped only when the prefix is empty *and* `leading_separator` is false.
fn group_name(prefix: &str, separator: &str, bare: &str, leading_separator: bool) -> String {
    let sep = if prefix.is_empty() && !leading_separator {
        ""
    } else {
        separator
    };
    crate::sanitize::to_safe_group_name(&format!("{prefix}{sep}{bare}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// ansible `%s`-formats a `groups`/`compose` value into a Jinja template, so a YAML
    /// scalar that parses as a non-string is accepted and stringified rather than rejected.
    #[test]
    fn non_string_group_and_compose_values_are_coerced() {
        let value = json!({
            "plugin": "constructed",
            "groups": { "dpu": true, "off": false, "n": 1 },
            "compose": { "flag": true },
        });
        let cfg: Config = serde_json::from_value(value).expect("non-string scalars accepted");
        assert_eq!(cfg.groups.get("dpu").map(String::as_str), Some("True"));
        assert_eq!(cfg.groups.get("off").map(String::as_str), Some("False"));
        assert_eq!(cfg.groups.get("n").map(String::as_str), Some("1"));
        assert_eq!(cfg.compose.get("flag").map(String::as_str), Some("True"));
        // The stringified forms evaluate as ansible would: `True` is an always-true group.
        let vars = serde_json::Map::new();
        assert!(expr::eval_condition(cfg.groups.get("dpu").unwrap(), &vars).unwrap());
        assert!(!expr::eval_condition(cfg.groups.get("off").unwrap(), &vars).unwrap());
    }

    /// A list/dict value still can't be a single expression — surface that, don't coerce.
    #[test]
    fn container_group_value_is_rejected() {
        let value = json!({ "plugin": "constructed", "groups": { "bad": [1, 2] } });
        assert!(serde_json::from_value::<Config>(value).is_err());
    }
}
