//! Variable precedence resolution, mirroring ansible's `VariableManager.get_vars`
//! for the inventory-only case (no playbook, no facts).
//!
//! Precedence, lowest to highest (`vars/manager.py`):
//!   1. `all` group vars (inventory source)            — `all_inventory`
//!   2. `all` group_vars/ files                        — `all_plugins_inventory`  (M4)
//!   3. other groups' inventory vars, sorted by         — `groups_inventory`
//!      `(depth, priority, name)` via `sort_groups`
//!   4. other groups' group_vars/ files                — `groups_plugins_inventory` (M4)
//!   5. host inventory vars                             — host.get_vars()
//!   6. host_vars/ files                               — inventory host_vars (M4)
//!
//! `combine_vars` with the default `hash_behaviour=replace` is a shallow top-level
//! overwrite (`a | b`), so [`combine`] just inserts/overwrites top-level keys.

use serde_json::{Map, Value};

use crate::model::{ALL, Group, InventoryData};

/// Keys ansible strips from `_meta.hostvars` output (`C.INTERNAL_STATIC_VARS`).
pub const INTERNAL_STATIC_VARS: &[&str] = &[
    "ansible_async_path",
    "ansible_collection_name",
    "ansible_config_file",
    "ansible_dependent_role_names",
    "ansible_diff_mode",
    "ansible_facts",
    "ansible_forks",
    "ansible_inventory_sources",
    "ansible_limit",
    "ansible_play_batch",
    "ansible_play_hosts",
    "ansible_play_hosts_all",
    "ansible_play_role_names",
    "ansible_playbook_python",
    "ansible_role_name",
    "ansible_role_names",
    "ansible_run_tags",
    "ansible_skip_tags",
    "ansible_verbosity",
    "ansible_version",
    "group_names",
    "groups",
    "hostvars",
    "inventory_dir",
    "inventory_file",
    "inventory_hostname",
    "inventory_hostname_short",
    "omit",
    "play_hosts",
    "playbook_dir",
    "role_name",
    "role_names",
    "role_path",
    "role_uuid",
];

/// `combine_vars(dst, src)` with default `replace` behaviour: top-level keys in `src`
/// overwrite those in `dst`; nested values are replaced wholesale (not merged).
pub fn combine(dst: &mut Map<String, Value>, src: &Map<String, Value>) {
    for (k, v) in src {
        dst.insert(k.clone(), v.clone());
    }
}

/// Order a host's groups (already excluding `all`) by ansible's precedence key.
fn sort_key(g: &Group) -> (u32, i64, &str) {
    (g.depth, g.priority, g.name.as_str())
}

/// A host's groups (excluding `all`) ordered by ansible's precedence key `(depth, priority,
/// name)`. The shared fold order for both full and inline-only resolution.
fn host_groups_sorted<'a>(inv: &'a InventoryData, host: &str) -> Vec<&'a Group> {
    let mut groups: Vec<&Group> = inv
        .host_groups(host)
        .into_iter()
        .filter(|g| g != ALL)
        .filter_map(|g| inv.groups.get(&g))
        .collect();
    groups.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));
    groups
}

/// Fully resolve a host's variables (group vars flattened onto the host), as used for
/// the non-export `_meta.hostvars` block. Does **not** strip internal static vars — the
/// output layer does that.
pub fn resolve_host_vars(inv: &InventoryData, host: &str) -> Map<String, Value> {
    let mut result = Map::new();

    // 1. `all` inventory vars, then 2. group_vars/all files.
    if let Some(all) = inv.groups.get(ALL) {
        combine(&mut result, &all.vars);
        combine(&mut result, &all.file_vars);
    }

    let groups = host_groups_sorted(inv, host);

    // 3. all groups' inventory vars (sorted fold), then 4. all groups' group_vars files
    // (sorted fold). Group-var files outrank every inventory-defined group var.
    for g in &groups {
        combine(&mut result, &g.vars);
    }
    for g in &groups {
        combine(&mut result, &g.file_vars);
    }

    // 5. host inventory vars, then 6. host_vars files (highest precedence).
    if let Some(h) = inv.hosts.get(host) {
        combine(&mut result, &h.vars);
        combine(&mut result, &h.file_vars);
    }

    result
}

/// Resolve a host's variables from **inventory-inline** sources only — the same precedence
/// fold as [`resolve_host_vars`] but excluding `group_vars/`/`host_vars/` *file* vars. This
/// is exactly what the `constructed` plugin sees, because in ansible it runs during parsing,
/// before the host_group_vars plugin layers those files in (verified differentially).
pub fn resolve_host_inline_vars(inv: &InventoryData, host: &str) -> Map<String, Value> {
    let mut result = Map::new();

    if let Some(all) = inv.groups.get(ALL) {
        combine(&mut result, &all.vars);
    }
    for g in host_groups_sorted(inv, host) {
        combine(&mut result, &g.vars);
    }
    if let Some(h) = inv.hosts.get(host) {
        combine(&mut result, &h.vars);
    }
    result
}

/// Remove ansible's internal/magic static vars from a resolved hostvars map.
pub fn remove_internal(vars: &mut Map<String, Value>) {
    for k in INTERNAL_STATIC_VARS {
        vars.remove(*k);
    }
}
