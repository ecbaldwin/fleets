//! Render the output `Value` to a string. JSON matches ansible's
//! `json.dumps(..., sort_keys=True, indent=4, ensure_ascii=False)`.
//!
//! `serde_json::Value` (default features) backs objects with a `BTreeMap`, so keys are
//! already sorted; we only need a 4-space pretty formatter to match the indent.

use serde::Serialize;
use serde_json::Value;
use serde_json::ser::{PrettyFormatter, Serializer};

use crate::error::{Error, Result};

/// Recursively sort object keys so display matches ansible's `sort_keys=True`. Array
/// element order is preserved (ansible does not sort list values). With serde_json's
/// `preserve_order` feature, `Value::Object` keeps insertion order, so we sort explicitly
/// for output while the build keeps source order (which drives traversal correctness).
fn sort_keys(value: &Value) -> Value {
    match value {
        Value::Object(m) => {
            let mut entries: Vec<(&String, Value)> =
                m.iter().map(|(k, v)| (k, sort_keys(v))).collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            Value::Object(entries.into_iter().map(|(k, v)| (k.clone(), v)).collect())
        }
        Value::Array(a) => Value::Array(a.iter().map(sort_keys).collect()),
        other => other.clone(),
    }
}

/// Serialize to JSON with 4-space indent and sorted keys (ansible-compatible).
pub fn to_json(value: &Value) -> String {
    let value = sort_keys(value);
    let mut buf = Vec::new();
    let formatter = PrettyFormatter::with_indent(b"    ");
    let mut ser = Serializer::with_formatter(&mut buf, formatter);
    value
        .serialize(&mut ser)
        .expect("serializing serde_json::Value cannot fail");
    String::from_utf8(buf).expect("serde_json emits valid UTF-8")
}

/// Serialize to YAML (block style, sorted keys), matching ansible's `yaml.dump`.
pub fn to_yaml(value: &Value) -> Result<String> {
    noyalib::to_string(&sort_keys(value)).map_err(|e| Error::Parse {
        path: "<output>".into(),
        msg: format!("cannot represent inventory as YAML: {e}"),
    })
}

/// Recursively drop `null` values: object keys whose value is null are removed and null
/// array elements are dropped. TOML has no null, and ansible's `toml_dumps` likewise omits
/// such values rather than erroring.
fn strip_nulls(value: &Value) -> Value {
    match value {
        Value::Object(m) => Value::Object(
            m.iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k.clone(), strip_nulls(v)))
                .collect(),
        ),
        Value::Array(a) => {
            Value::Array(a.iter().filter(|v| !v.is_null()).map(strip_nulls).collect())
        }
        other => other.clone(),
    }
}

/// Serialize to TOML. Mirrors ansible's `toml_dumps`, which omits `null` values (TOML has
/// no null representation).
pub fn to_toml(value: &Value) -> Result<String> {
    let value = strip_nulls(value);
    // Convert through `toml::Value` so table-ordering rules are handled correctly.
    let tv = toml::Value::try_from(&value).map_err(|e| Error::Parse {
        path: "<output>".into(),
        msg: format!("cannot represent inventory as TOML: {e}"),
    })?;
    toml::to_string_pretty(&tv).map_err(|e| Error::Parse {
        path: "<output>".into(),
        msg: format!("cannot serialize TOML: {e}"),
    })
}
