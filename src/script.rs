//! Dynamic (executable) inventory sources, mirroring ansible's `script` inventory plugin.
//!
//! The script is run once with `--list`; its JSON output maps group names to either a
//! `{hosts, vars, children}` object or a bare host-list shorthand, plus an optional
//! `_meta.hostvars`. When `_meta` is present it supplies host vars directly; otherwise the
//! script is run once per host with `--host <name>` (the legacy fallback contract).

use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{Map, Value};

use crate::error::{Error, Result};
use crate::model::InventoryData;

/// Is `path` a regular file with an executable bit set? (ansible's script plugin claims
/// executable files.) Non-unix platforms conservatively answer `false`.
#[cfg(unix)]
pub fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
pub fn is_executable(_path: &Path) -> bool {
    false
}

/// Parse a dynamic inventory script into the model: run it with `--list`, then ingest.
pub fn parse_script(inv: &mut InventoryData, path: &Path) -> Result<()> {
    let list = run_list(path)?;
    parse_list(inv, path, list)
}

/// Run a dynamic inventory script's `--list` and return its parsed JSON. This is the slow
/// part (a subprocess), split out so a directory inventory can run several concurrently
/// before ingesting them in order (see `parse::parse_directory`).
pub fn run_list(path: &Path) -> Result<Value> {
    run_script(path, &["--list"])
}

/// Ingest a script's already-fetched `--list` output into the model. Kept separate from
/// [`run_list`] so the (sequential, order-sensitive) model mutation is decoupled from the
/// (parallelizable) subprocess execution. The per-host `--host` fallback, if needed, still
/// runs here sequentially.
pub fn parse_list(inv: &mut InventoryData, path: &Path, list: Value) -> Result<()> {
    let top = list.as_object().ok_or_else(|| Error::Parse {
        path: path.display().to_string(),
        msg: "inventory script --list output must be a JSON object".into(),
    })?;

    // Hosts in first-seen order, and the optional _meta.hostvars block.
    let mut hosts: Vec<String> = Vec::new();
    let mut meta: Option<&Map<String, Value>> = None;
    for (group, gdata) in top {
        if group == "_meta" {
            meta = gdata.get("hostvars").and_then(|h| h.as_object());
        } else {
            parse_group(inv, group, gdata, &mut hosts);
        }
    }

    // Apply host vars: from _meta if present, else by calling the script per host.
    for host in &hosts {
        let vars = match meta {
            Some(m) => m
                .get(host)
                .and_then(|v| v.as_object())
                .cloned()
                .unwrap_or_default(),
            None => run_script(path, &["--host", host])?
                .as_object()
                .cloned()
                .unwrap_or_default(),
        };
        for (k, v) in vars {
            inv.set_host_var(host, &k, v);
        }
    }
    Ok(())
}

/// Mirror ansible's `_parse_group`: normalize the shorthand shapes, then add hosts, vars,
/// and children.
fn parse_group(inv: &mut InventoryData, group: &str, data: &Value, hosts: &mut Vec<String>) {
    let safe = inv.add_group(group);

    // Normalize: a non-object value is a bare host list; an object lacking the known keys
    // is "simplified syntax" — a single host named after the group, carrying those vars.
    let normalized: Map<String, Value> = match data {
        Value::Object(o)
            if o.contains_key("hosts") || o.contains_key("vars") || o.contains_key("children") =>
        {
            o.clone()
        }
        Value::Object(o) => {
            let mut m = Map::new();
            m.insert(
                "hosts".into(),
                Value::Array(vec![Value::String(group.to_string())]),
            );
            m.insert("vars".into(), Value::Object(o.clone()));
            m
        }
        other => {
            let mut m = Map::new();
            m.insert("hosts".into(), other.clone());
            m
        }
    };

    if let Some(Value::Array(list)) = normalized.get("hosts") {
        for h in list {
            if let Some(name) = h.as_str() {
                inv.add_host(name);
                inv.add_host_to_group(name, &safe);
                if !hosts.iter().any(|x| x == name) {
                    hosts.push(name.to_string());
                }
            }
        }
    }
    if let Some(Value::Object(vars)) = normalized.get("vars") {
        for (k, v) in vars {
            inv.set_group_var(&safe, k, v.clone());
        }
    }
    if let Some(Value::Array(children)) = normalized.get("children") {
        for c in children {
            if let Some(child) = c.as_str() {
                let cs = inv.add_group(child);
                inv.add_child(&safe, &cs);
            }
        }
    }
}

/// Run the inventory script with the given args and parse its stdout as JSON.
///
/// stderr is inherited so the script's diagnostics flow straight to the terminal, matching
/// `ansible-inventory` (which does not swallow a script's stderr). Only stdout is captured,
/// since that carries the JSON we parse.
fn run_script(path: &Path, args: &[&str]) -> Result<Value> {
    // `Command` treats a bare relative name as a program to search for on PATH. Inventory
    // sources are filesystem paths, so make that case explicitly relative to the current
    // directory instead (matching ansible-inventory and avoiding execution of a namesake
    // program found elsewhere on PATH).
    let command_path = if path.is_relative() && path.components().count() == 1 {
        Path::new(".").join(path)
    } else {
        path.to_path_buf()
    };
    let output = Command::new(command_path)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .and_then(|child| child.wait_with_output())
        .map_err(|e| Error::Io {
            path: path.display().to_string(),
            source: e,
        })?;
    if !output.status.success() {
        return Err(Error::Parse {
            path: path.display().to_string(),
            msg: format!("inventory script {args:?} exited with {}", output.status),
        });
    }
    serde_json::from_slice(&output.stdout).map_err(|e| Error::Parse {
        path: path.display().to_string(),
        msg: format!("invalid JSON from script {args:?}: {e}"),
    })
}
