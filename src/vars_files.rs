//! Adjacent `group_vars/` and `host_vars/` loading, mirroring ansible's
//! `host_group_vars` vars plugin and `DataLoader.find_vars_files`.
//!
//! For each inventory source we derive a base directory (the directory itself if the
//! source is a directory, else its parent) and look for `<base>/group_vars/<group>` and
//! `<base>/host_vars/<host>`. Each match may be a single file or a directory of files
//! merged together. Loaded vars land in each entity's `file_vars`, which
//! [`crate::vars::resolve_host_vars`] applies above the corresponding inventory-source vars.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::error::{Error, Result};
use crate::model::InventoryData;

/// Extensions tried for a `group_vars/<name>` / `host_vars/<name>` lookup, in order. The
/// empty extension is first so a `<name>` directory is found before a `<name>.yml` file.
const VAR_EXTS: &[&str] = &["", ".yml", ".yaml", ".json"];

/// Load all adjacent `group_vars`/`host_vars` for every group and host in `inv`.
pub fn load_inventory_vars(inv: &mut InventoryData, sources: &[&Path]) -> Result<()> {
    let basedirs = basedirs(sources);

    let group_names: Vec<String> = inv.groups.keys().cloned().collect();
    for name in group_names {
        let merged = collect_entity_vars(&basedirs, "group_vars", &name)?;
        if !merged.is_empty() {
            let g = inv.group_mut(&name);
            for (k, v) in merged {
                g.file_vars.insert(k, v);
            }
        }
    }

    let host_names: Vec<String> = inv.hosts.keys().cloned().collect();
    for name in host_names {
        let merged = collect_entity_vars(&basedirs, "host_vars", &name)?;
        if !merged.is_empty() {
            let h = inv.host_mut(&name);
            for (k, v) in merged {
                h.file_vars.insert(k, v);
            }
        }
    }
    Ok(())
}

/// Base directories to search, de-duplicated in source order: the source itself if it is a
/// directory, otherwise its parent directory.
fn basedirs(sources: &[&Path]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for s in sources {
        let base = if s.is_dir() {
            s.to_path_buf()
        } else {
            s.parent()
                .filter(|p| !p.as_os_str().is_empty())
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."))
        };
        if !out.contains(&base) {
            out.push(base);
        }
    }
    out
}

/// Merge an entity's vars across every base dir (in order), each base contributing its
/// found files (in order).
fn collect_entity_vars(
    basedirs: &[PathBuf],
    subdir: &str,
    name: &str,
) -> Result<Map<String, Value>> {
    let mut merged = Map::new();
    for base in basedirs {
        for file in find_vars_files(&base.join(subdir), name) {
            if let Some(map) = load_vars_file(&file)? {
                for (k, v) in map {
                    merged.insert(k, v);
                }
            }
        }
    }
    Ok(merged)
}

/// Find the vars file(s) for `name` under `dir` (a `group_vars`/`host_vars` directory).
/// Returns the single matching file, or every file inside a matching `<name>/` directory.
fn find_vars_files(dir: &Path, name: &str) -> Vec<PathBuf> {
    // Entity names are inventory data, not paths. Requiring exactly one normal component
    // prevents names such as `../secrets` from escaping the adjacent vars directory.
    let mut components = Path::new(name).components();
    let safe_name = matches!(components.next(), Some(std::path::Component::Normal(_)))
        && components.next().is_none();
    if !safe_name || !dir.is_dir() {
        return Vec::new();
    }
    for ext in VAR_EXTS {
        let candidate = dir.join(format!("{name}{ext}"));
        if !candidate.exists() {
            continue;
        }
        if candidate.is_dir() {
            // Only the extension-less name resolves to a directory of files.
            if ext.is_empty() {
                return dir_vars_files(&candidate);
            }
            break;
        }
        return vec![candidate];
    }
    Vec::new()
}

/// All var files inside a `group_vars/<name>/` (or `host_vars/<name>/`) directory: sorted,
/// skipping hidden/backup entries, taking files with no extension or a `.yml/.yaml/.json`
/// extension, and recursing into extension-less subdirectories.
fn dir_vars_files(dir: &Path) -> Vec<PathBuf> {
    let mut entries: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(|e| e.ok().map(|e| e.path())).collect(),
        Err(_) => return Vec::new(),
    };
    entries.sort();
    let mut found = Vec::new();
    for entry in entries {
        let fname = entry
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        if fname.starts_with('.') || fname.ends_with('~') {
            continue;
        }
        let ext = entry.extension().and_then(|e| e.to_str());
        if entry.is_dir() {
            if ext.is_none() {
                found.extend(dir_vars_files(&entry));
            }
        } else if matches!(ext, None | Some("yml") | Some("yaml") | Some("json")) {
            found.push(entry);
        }
    }
    found
}

/// Load a vars file as a YAML/JSON mapping. Empty files yield `None`; a non-mapping
/// top-level document is treated as empty (ansible would error, but we stay lenient).
fn load_vars_file(path: &Path) -> Result<Option<Map<String, Value>>> {
    let text = std::fs::read_to_string(path).map_err(|e| Error::Io {
        path: path.display().to_string(),
        source: e,
    })?;
    if text.trim().is_empty() {
        return Ok(None);
    }
    let value: Value = noyalib::from_str(&text).map_err(|e| Error::Parse {
        path: path.display().to_string(),
        msg: e.to_string(),
    })?;
    Ok(value.as_object().cloned())
}
