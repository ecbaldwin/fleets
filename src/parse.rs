//! Inventory source parsing: structured (YAML/JSON), TOML, INI, directory, and dynamic
//! executable (script) sources, plus adjacent `group_vars/`/`host_vars/` loading.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Map, Value};

use crate::enabled::EnabledKinds;
use crate::error::{Error, Result};
use crate::model::InventoryData;
use crate::{script, vars_files};

/// Load one or more inventory sources into a single merged [`InventoryData`].
///
/// Sources are parsed in order and merged into the same model (matching ansible's
/// behaviour of layering multiple `-i` sources). Then groups inherit from `all`, hosts
/// fall into `ungrouped`, and adjacent `group_vars/`/`host_vars/` files are loaded.
pub fn load_sources(paths: &[impl AsRef<Path>], enabled: EnabledKinds) -> Result<InventoryData> {
    let _span = tracing::info_span!("load_inventory", sources = paths.len()).entered();
    let mut inv = InventoryData::new();
    let mut active_dirs = HashSet::new();
    for p in paths {
        parse_source(&mut inv, p.as_ref(), enabled, &mut active_dirs)?;
    }
    inv.reconcile_all_children();
    inv.reconcile_ungrouped();
    let source_paths: Vec<&Path> = paths.iter().map(|p| p.as_ref()).collect();
    vars_files::load_inventory_vars(&mut inv, &source_paths)?;
    Ok(inv)
}

fn parse_source(
    inv: &mut InventoryData,
    path: &Path,
    enabled: EnabledKinds,
    active_dirs: &mut HashSet<PathBuf>,
) -> Result<()> {
    let p = path.display();
    if path.is_dir() {
        let canonical = std::fs::canonicalize(path).map_err(|e| Error::Io {
            path: path.display().to_string(),
            source: e,
        })?;
        if !active_dirs.insert(canonical.clone()) {
            return Err(Error::Parse {
                path: path.display().to_string(),
                msg: "directory inventory contains a symlink cycle".into(),
            });
        }
        // Each entry parsed inside gets its own child span (see `parse_directory`).
        let _span = tracing::info_span!("parse_directory", path = %p).entered();
        let result = parse_directory(inv, path, enabled, active_dirs);
        active_dirs.remove(&canonical);
        return result;
    }
    // A dynamic inventory: an executable file with no inventory extension. When the `script`
    // kind is disabled, such a file is left unparsed (skipped) rather than executed — we do
    // not fall back to interpreting an executable as YAML.
    if !has_inventory_ext(path) && script::is_executable(path) {
        if !enabled.script {
            return skip_disabled("script", path);
        }
        let _span = tracing::info_span!("parse_script", path = %p).entered();
        return script::parse_script(inv, path);
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        // ansible's yaml plugin accepts .yaml/.yml/.json (YAML is a JSON superset). A
        // structured file may turn out to be a `constructed` config — that adds its own
        // child span inside `parse_structured_file`.
        "yaml" | "yml" | "json" | "" => {
            if !enabled.yaml {
                return skip_disabled("yaml", path);
            }
            let _span = tracing::info_span!("parse_yaml", path = %p).entered();
            parse_structured_file(inv, path, enabled)
        }
        "toml" => {
            if !enabled.toml {
                return skip_disabled("toml", path);
            }
            let _span = tracing::info_span!("parse_toml", path = %p).entered();
            parse_toml_file(inv, path)
        }
        "ini" => {
            if !enabled.ini {
                return skip_disabled("ini", path);
            }
            let _span = tracing::info_span!("parse_ini", path = %p).entered();
            crate::ini::parse_ini_file(inv, path)
        }
        other => Err(Error::UnsupportedSource {
            path: path.display().to_string(),
            ext: other.to_string(),
        }),
    }
}

/// A source whose kind is disabled via `ANSIBLE_INVENTORY_ENABLED` is skipped, not errored —
/// matching ansible, which leaves such a source "unparsed" (a warning by default).
fn skip_disabled(kind: &str, path: &Path) -> Result<()> {
    tracing::warn!(
        path = %path.display(),
        kind,
        "source kind not in ANSIBLE_INVENTORY_ENABLED; skipping"
    );
    Ok(())
}

fn has_inventory_ext(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str(),
        "yaml" | "yml" | "json" | "toml" | "ini"
    )
}

/// Parse a directory inventory: every non-ignored entry, in codepoint-sorted order, is parsed
/// as its own source (recursing into subdirectories). Mirrors `InventoryManager`'s directory
/// handling and its ignore rules.
///
/// Dynamic-script sources are the slow ones (each runs a subprocess), so we run their `--list`
/// **concurrently** up front, then ingest every source **sequentially in sorted order**. The
/// ingest order is unchanged from a fully sequential parse, so variable precedence, host
/// insertion order, and — crucially — the `constructed` plugin's "sees only the hosts loaded
/// before it" semantics are byte-for-byte identical (see `docs/anomalies.md` §28).
///
/// One subtle difference from sequential parsing: because all scripts are run up front, a
/// script may execute even if an earlier-sorted source would later abort the parse. Inventory
/// scripts are read-only queries, so this is benign; the reported error is still the first
/// failing source in sorted order.
fn parse_directory(
    inv: &mut InventoryData,
    dir: &Path,
    enabled: EnabledKinds,
    active_dirs: &mut HashSet<PathBuf>,
) -> Result<()> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| Error::Io {
            path: dir.display().to_string(),
            source: e,
        })?
        .map(|entry| {
            entry.map(|entry| entry.path()).map_err(|e| Error::Io {
                path: dir.display().to_string(),
                source: e,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    entries.sort();
    entries.retain(|e| {
        let name = e.file_name().and_then(|n| n.to_str()).unwrap_or("");
        !is_ignored_dir_entry(name)
    });

    // The dynamic-script entries, in sorted order. These are what we run in parallel — but
    // only when the `script` kind is enabled; otherwise they fall through to extension
    // dispatch like any other entry.
    let scripts: Vec<PathBuf> = if enabled.script {
        entries
            .iter()
            .filter(|e| !e.is_dir() && !has_inventory_ext(e) && script::is_executable(e))
            .cloned()
            .collect()
    } else {
        Vec::new()
    };

    // Only bother with the executor when there's more than one script to overlap; a single
    // (or zero) script keeps the simple inline path (its own `parse_script` span and all).
    let mut prefetched: HashMap<PathBuf, Result<Value>> = if scripts.len() > 1 {
        run_scripts_parallel(&scripts)
    } else {
        HashMap::new()
    };

    for entry in entries {
        match prefetched.remove(&entry) {
            // Subprocess already ran concurrently; just ingest it now, in order.
            Some(list) => {
                let _span =
                    tracing::info_span!("parse_script", path = %entry.display(), parallel = true)
                        .entered();
                script::parse_list(inv, &entry, list?)?;
            }
            None => parse_source(inv, &entry, enabled, active_dirs)?,
        }
    }
    Ok(())
}

/// Run several inventory scripts' `--list` concurrently on async-std's blocking thread pool,
/// returning each path's result. Each run gets its own span parented to the current
/// (`parse_directory`) span, so the trace shows the runs overlapping in time.
fn run_scripts_parallel(paths: &[PathBuf]) -> HashMap<PathBuf, Result<Value>> {
    let parent = tracing::Span::current();
    let next = AtomicUsize::new(0);
    let results = Mutex::new(HashMap::with_capacity(paths.len()));
    let workers = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .min(paths.len());

    std::thread::scope(|scope| {
        for _ in 0..workers {
            let parent = parent.clone();
            let next = &next;
            let results = &results;
            scope.spawn(move || {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(path) = paths.get(index).cloned() else {
                        break;
                    };
                    let span =
                        tracing::info_span!(parent: &parent, "run_script", path = %path.display());
                    let _enter = span.enter();
                    let result = script::run_list(&path);
                    results
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(path, result);
                }
            });
        }
    });

    results
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Directory entries ansible's `IGNORED` regex skips: dotfiles, the `group_vars`/
/// `host_vars`/`vars_plugins` subdirs, backups, and editor/doc extensions (incl. `.ini`/`.cfg`).
fn is_ignored_dir_entry(name: &str) -> bool {
    if name.starts_with('.') || name.ends_with('~') {
        return true;
    }
    if matches!(name, "host_vars" | "group_vars" | "vars_plugins") {
        return true;
    }
    const IGNORED_EXTS: &[&str] = &[
        ".pyc", ".pyo", ".swp", ".bak", ".rpm", ".md", ".txt", ".rst", ".orig", ".ini", ".cfg",
        ".retry",
    ];
    IGNORED_EXTS.iter().any(|e| name.ends_with(e))
}

/// Parse a structured (YAML/JSON) inventory file into the model.
pub fn parse_structured_file(
    inv: &mut InventoryData,
    path: &Path,
    enabled: EnabledKinds,
) -> Result<()> {
    let text = std::fs::read_to_string(path).map_err(|e| Error::Io {
        path: path.display().to_string(),
        source: e,
    })?;
    let value: Value = noyalib::from_str(&text).map_err(|e| Error::Parse {
        path: path.display().to_string(),
        msg: e.to_string(),
    })?;
    // The `constructed` plugin is a post-processing pass over the graph built so far, not a
    // structural document. Routing on its `plugin:` key is ansible's `auto` plugin's job, so
    // we only do so when the `auto` kind is enabled; otherwise the file is parsed as an
    // ordinary structured document (as ansible's `yaml` plugin would). Running it here — at
    // this source's position in the parse stream — gives it the correct incremental
    // visibility for directory and multi-`-i` inventories.
    if enabled.auto && crate::constructed::is_constructed(&value) {
        return crate::constructed::apply(inv, &value, path);
    }
    parse_structured_value(inv, &value, path)
}

/// Parse a TOML inventory file. ansible's TOML schema matches the structured (group ->
/// {hosts, vars, children}) shape, except `children` is a list of group names rather than
/// a nested map. We decode to the common `Value` model and reuse the structured parser.
pub fn parse_toml_file(inv: &mut InventoryData, path: &Path) -> Result<()> {
    let text = std::fs::read_to_string(path).map_err(|e| Error::Io {
        path: path.display().to_string(),
        source: e,
    })?;
    let value: Value = toml::from_str(&text).map_err(|e| Error::Parse {
        path: path.display().to_string(),
        msg: e.to_string(),
    })?;
    parse_structured_value(inv, &value, path)
}

/// Parse an already-decoded structured inventory document.
pub fn parse_structured_value(inv: &mut InventoryData, value: &Value, path: &Path) -> Result<()> {
    let top = value.as_object().ok_or_else(|| Error::Parse {
        path: path.display().to_string(),
        msg: "inventory file must be a mapping of group names to group definitions".into(),
    })?;
    for (group_name, group_def) in top {
        parse_group(inv, group_name, group_def, None);
    }
    Ok(())
}

/// Parse a single group definition and its nested children.
///
/// `parent` is the (already-sanitized) parent group name, or `None` for a top-level
/// group — top-level groups other than `all` become children of `all`.
fn parse_group(inv: &mut InventoryData, raw_name: &str, def: &Value, parent: Option<&str>) {
    let name = inv.add_group(raw_name);

    // Only an explicit parent is linked here. Top-level groups are linked to `all` later,
    // in reconcile, and only if they end up with no other parent (so a group that is both
    // a top-level table and another group's child is NOT also a child of `all`).
    if let Some(p) = parent {
        inv.add_child(p, &name);
    }

    let obj = match def {
        Value::Object(o) => o,
        // `null` (or anything else) means an empty group; nothing more to do.
        _ => return,
    };

    if let Some(hosts) = obj.get("hosts") {
        parse_hosts(inv, &name, hosts);
    }
    if let Some(Value::Object(vars)) = obj.get("vars") {
        merge_group_vars(inv, &name, vars);
    }
    match obj.get("children") {
        // YAML/JSON: nested map of subgroup name -> definition.
        Some(Value::Object(children)) => {
            for (sub_name, sub_def) in children {
                parse_group(inv, sub_name, sub_def, Some(&name));
            }
        }
        // TOML: list of subgroup names; the subgroups are defined as separate tables.
        Some(Value::Array(children)) => {
            for sub in children {
                if let Some(sub_name) = sub.as_str() {
                    let safe = inv.add_group(sub_name);
                    inv.add_child(&name, &safe);
                }
            }
        }
        _ => {}
    }
}

/// Add the hosts of a group. Hosts may be a mapping (hostname -> vars) or a sequence of
/// bare hostnames.
fn parse_hosts(inv: &mut InventoryData, group: &str, hosts: &Value) {
    match hosts {
        Value::Object(map) => {
            for (host, host_vars) in map {
                add_host(inv, group, host, host_vars.as_object());
            }
        }
        Value::Array(list) => {
            for h in list {
                if let Some(name) = h.as_str() {
                    add_host(inv, group, name, None);
                }
            }
        }
        _ => {}
    }
}

fn add_host(inv: &mut InventoryData, group: &str, host: &str, vars: Option<&Map<String, Value>>) {
    inv.add_host(host);
    inv.add_host_to_group(host, group);
    if let Some(v) = vars {
        let h = inv.host_mut(host);
        crate::vars::combine(&mut h.vars, v);
    }
}

/// Merge a group's vars one key at a time so `ansible_group_priority` is consumed into the
/// group's priority field (and not kept as a normal var) — see [`InventoryData::set_group_var`].
fn merge_group_vars(inv: &mut InventoryData, group: &str, vars: &Map<String, Value>) {
    for (k, v) in vars {
        inv.set_group_var(group, k, v.clone());
    }
}
