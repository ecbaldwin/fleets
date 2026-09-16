//! Output builders. Each mirrors a `*_inventory` function in ansible's `cli/inventory.py`.
//! M1 implements the default `--list` JSON tree and `_meta.hostvars`; YAML/TOML trees and
//! `--graph` come later. All builders produce a `serde_json::Value` that the serializer
//! layer renders to JSON/YAML/TOML.

use std::collections::HashSet;

use serde_json::{Map, Value};

use crate::model::{ALL, InventoryData, UNGROUPED};
use crate::vars::{remove_internal, resolve_host_vars};

/// Controls whether list output prunes groups based on the hosts that remain visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupPruning {
    /// Keep only groups with visible hosts or exported vars, plus their ancestors.
    GraphAware,
    /// Preserve ansible-inventory's shallow pruning and dangling child references.
    AnsibleCompatible,
}

/// Resolved hostvars (or own vars under export), internal keys stripped.
fn host_vars_for(inv: &InventoryData, host: &str, export: bool) -> Map<String, Value> {
    let mut hv = if export {
        host_own_vars(inv, host)
    } else {
        resolve_host_vars(inv, host)
    };
    remove_internal(&mut hv);
    hv
}

/// Set of host names visible after optional `--limit` filtering.
pub type Available<'a> = &'a HashSet<String>;

/// Groups that contribute something to graph-aware output. Start with groups that have a
/// visible direct host (or exported vars), then retain every ancestor needed to connect them.
fn retained_groups(inv: &InventoryData, available: Available, export: bool) -> HashSet<String> {
    let mut retained = HashSet::new();
    let mut pending = Vec::new();

    for (name, group) in &inv.groups {
        let has_visible_host = group.hosts.iter().any(|host| available.contains(host));
        let has_exported_vars = export && !group_export_vars(inv, name).is_empty();
        if has_visible_host || has_exported_vars {
            retained.insert(name.clone());
            pending.push(name.clone());
        }
    }

    while let Some(name) = pending.pop() {
        let Some(group) = inv.groups.get(&name) else {
            continue;
        };
        for parent in &group.parents {
            if retained.insert(parent.clone()) {
                pending.push(parent.clone());
            }
        }
    }

    retained
}

fn retained(groups: Option<&HashSet<String>>, group: &str) -> bool {
    groups.is_none_or(|groups| groups.contains(group))
}

/// Build the default `--list` JSON structure: the recursive group tree plus
/// `_meta.hostvars`. `export` switches to the round-trippable var placement.
pub fn build_list(
    inv: &InventoryData,
    available: Available,
    export: bool,
    pruning: GroupPruning,
) -> Value {
    let mut results = Map::new();
    let mut seen = HashSet::new();
    let retained_groups = match pruning {
        GroupPruning::GraphAware => Some(retained_groups(inv, available, export)),
        GroupPruning::AnsibleCompatible => None,
    };
    format_group(
        inv,
        ALL,
        available,
        export,
        retained_groups.as_ref(),
        &mut seen,
        &mut results,
    );

    // _meta.hostvars — present even when empty.
    let mut hostvars = Map::new();
    for name in inv.hosts.keys() {
        if !available.contains(name) {
            continue;
        }
        let mut hv = if export {
            host_own_vars(inv, name)
        } else {
            resolve_host_vars(inv, name)
        };
        remove_internal(&mut hv);
        if !hv.is_empty() {
            hostvars.insert(name.clone(), Value::Object(hv));
        }
    }
    let mut meta = Map::new();
    meta.insert("hostvars".into(), Value::Object(hostvars));
    results.insert("_meta".into(), Value::Object(meta));

    Value::Object(results)
}

/// Recursively format `group` and its children into `out`, mirroring
/// `json_inventory.format_group`. Empty keys/groups are pruned by only inserting
/// non-empty values.
fn format_group(
    inv: &InventoryData,
    group: &str,
    available: Available,
    export: bool,
    retained_groups: Option<&HashSet<String>>,
    seen: &mut HashSet<String>,
    out: &mut Map<String, Value>,
) {
    let g = match inv.groups.get(group) {
        Some(g) => g,
        None => return,
    };
    let mut entry = Map::new();

    // `all` never lists its own hosts.
    if group != ALL {
        let hosts: Vec<Value> = g
            .hosts
            .iter()
            .filter(|h| available.contains(*h))
            .map(|h| Value::String(h.clone()))
            .collect();
        if !hosts.is_empty() {
            entry.insert("hosts".into(), Value::Array(hosts));
        }
    }

    let mut children = Vec::new();
    for sub in &g.children {
        if !retained(retained_groups, sub) {
            continue;
        }
        children.push(Value::String(sub.clone()));
        if seen.insert(sub.clone()) {
            format_group(inv, sub, available, export, retained_groups, seen, out);
        }
    }
    if !children.is_empty() {
        entry.insert("children".into(), Value::Array(children));
    }

    if export {
        let gv = group_export_vars(inv, group);
        if !gv.is_empty() {
            entry.insert("vars".into(), Value::Object(gv));
        }
    }

    if !entry.is_empty() {
        out.insert(group.to_string(), Value::Object(entry));
    }
}

/// `--host` output: the resolved (or own, under export) vars for a single host, with
/// internal vars stripped. Returns `None` if the host is unknown.
pub fn build_host(inv: &InventoryData, host: &str, export: bool) -> Option<Value> {
    if !inv.hosts.contains_key(host) {
        return None;
    }
    let mut hv = if export {
        host_own_vars(inv, host)
    } else {
        resolve_host_vars(inv, host)
    };
    remove_internal(&mut hv);
    Some(Value::Object(hv))
}

/// Under `--export`, a host carries its own inventory vars plus its `host_vars/` files
/// (mirrors `_get_host_variables`'s export branch), but no inherited group vars.
fn host_own_vars(inv: &InventoryData, host: &str) -> Map<String, Value> {
    let mut out = Map::new();
    if let Some(h) = inv.hosts.get(host) {
        crate::vars::combine(&mut out, &h.vars);
        crate::vars::combine(&mut out, &h.file_vars);
    }
    out
}

// ---------------------------------------------------------------------------
// YAML tree (`yaml_inventory`): a single nested `all:` tree. `children` and `hosts`
// are MAPS; host vars are emitted only at a host's first occurrence (seen-host dedup),
// so traversal order (child insertion order) decides which group carries the vars.
// ---------------------------------------------------------------------------

pub fn build_yaml(
    inv: &InventoryData,
    available: Available,
    export: bool,
    pruning: GroupPruning,
) -> Value {
    let mut seen_hosts = HashSet::new();
    let mut seen_groups = HashSet::new();
    let retained_groups = match pruning {
        GroupPruning::GraphAware => Some(retained_groups(inv, available, export)),
        GroupPruning::AnsibleCompatible => None,
    };
    let map = format_group_yaml(
        inv,
        ALL,
        available,
        export,
        retained_groups.as_ref(),
        &mut seen_hosts,
        &mut seen_groups,
    );
    Value::Object(map)
}

/// Returns `{group: {...}}`, or an empty map if the group is pruned.
fn format_group_yaml(
    inv: &InventoryData,
    group: &str,
    available: Available,
    export: bool,
    retained_groups: Option<&HashSet<String>>,
    seen_hosts: &mut HashSet<String>,
    seen_groups: &mut HashSet<String>,
) -> Map<String, Value> {
    let g = match inv.groups.get(group) {
        Some(g) => g,
        None => return Map::new(),
    };
    let mut entry = Map::new();

    let mut children = Map::new();
    for sub in &g.children {
        if sub == ALL {
            continue;
        }
        if !retained(retained_groups, sub) {
            continue;
        }
        if seen_groups.contains(sub) {
            children.insert(sub.clone(), Value::Object(Map::new()));
        } else {
            seen_groups.insert(sub.clone());
            let sub_map = format_group_yaml(
                inv,
                sub,
                available,
                export,
                retained_groups,
                seen_hosts,
                seen_groups,
            );
            for (k, v) in sub_map {
                children.insert(k, v);
            }
        }
    }
    if !children.is_empty() {
        entry.insert("children".into(), Value::Object(children));
    }

    if group != ALL {
        let mut hosts = Map::new();
        for h in &g.hosts {
            if !available.contains(h) {
                continue;
            }
            let myvars = if seen_hosts.insert(h.clone()) {
                Value::Object(host_vars_for(inv, h, export))
            } else {
                Value::Object(Map::new())
            };
            hosts.insert(h.clone(), myvars);
        }
        if !hosts.is_empty() {
            entry.insert("hosts".into(), Value::Object(hosts));
        }
    }

    if export {
        let gv = group_export_vars(inv, group);
        if !gv.is_empty() {
            entry.insert("vars".into(), Value::Object(gv));
        }
    }

    if entry.is_empty() {
        return Map::new();
    }
    let mut out = Map::new();
    out.insert(group.to_string(), Value::Object(entry));
    out
}

// ---------------------------------------------------------------------------
// TOML tree (`toml_inventory`): a FLAT map of groups. `children` is a LIST; `hosts`
// is a MAP. `all` lists no children (so it is usually pruned). `ungrouped` is omitted
// as a child unless it actually has hosts.
// ---------------------------------------------------------------------------

pub fn build_toml(
    inv: &InventoryData,
    available: Available,
    export: bool,
    pruning: GroupPruning,
) -> Value {
    let has_ungrouped = inv
        .groups
        .get(UNGROUPED)
        .is_some_and(|g| !g.hosts.is_empty());
    let seen_hosts = HashSet::new();
    let out = Map::new();
    let retained_groups = match pruning {
        GroupPruning::GraphAware => Some(retained_groups(inv, available, export)),
        GroupPruning::AnsibleCompatible => None,
    };
    let mut state = TomlFormatState {
        available,
        export,
        retained_groups: retained_groups.as_ref(),
        has_ungrouped,
        active_groups: HashSet::from([ALL.to_string()]),
        seen_hosts,
        out,
    };
    format_group_toml(inv, ALL, &mut state);
    Value::Object(state.out)
}

struct TomlFormatState<'a> {
    available: Available<'a>,
    export: bool,
    retained_groups: Option<&'a HashSet<String>>,
    has_ungrouped: bool,
    active_groups: HashSet<String>,
    seen_hosts: HashSet<String>,
    out: Map<String, Value>,
}

fn format_group_toml(inv: &InventoryData, group: &str, state: &mut TomlFormatState<'_>) {
    let g = match inv.groups.get(group) {
        Some(g) => g,
        None => return,
    };
    let mut entry = Map::new();

    let mut children = Vec::new();
    for sub in &g.children {
        if !retained(state.retained_groups, sub) {
            continue;
        }
        if state.retained_groups.is_none() && sub == UNGROUPED && !state.has_ungrouped {
            continue;
        }
        if group != ALL {
            children.push(Value::String(sub.clone()));
        }
        if state.active_groups.insert(sub.clone()) {
            format_group_toml(inv, sub, state);
            state.active_groups.remove(sub);
        }
    }
    if !children.is_empty() {
        entry.insert("children".into(), Value::Array(children));
    }

    if group != ALL {
        let mut hosts = Map::new();
        for h in &g.hosts {
            if !state.available.contains(h) {
                continue;
            }
            let myvars = if state.seen_hosts.insert(h.clone()) {
                Value::Object(host_vars_for(inv, h, state.export))
            } else {
                Value::Object(Map::new())
            };
            hosts.insert(h.clone(), myvars);
        }
        if !hosts.is_empty() {
            entry.insert("hosts".into(), Value::Object(hosts));
        }
    }

    if state.export {
        let gv = group_export_vars(inv, group);
        if !gv.is_empty() {
            entry.insert("vars".into(), Value::Object(gv));
        }
    }

    if !entry.is_empty() {
        state.out.insert(group.to_string(), Value::Object(entry));
    }
}

/// Group vars for `--export`, mirroring `_get_group_variables`: source vars, plus
/// `ansible_group_priority` re-added when the priority isn't the default 1.
fn group_export_vars(inv: &InventoryData, group: &str) -> Map<String, Value> {
    let g = match inv.groups.get(group) {
        Some(g) => g,
        None => return Map::new(),
    };
    // Source vars plus group_vars/ files (mirrors `_get_group_variables`).
    let mut res = g.vars.clone();
    crate::vars::combine(&mut res, &g.file_vars);
    if g.priority != 1 {
        res.insert("ansible_group_priority".into(), Value::from(g.priority));
    }
    remove_internal(&mut res);
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_inventory() -> InventoryData {
        let mut inv = InventoryData::new();
        for group in [
            "empty_leaf",
            "empty_branch",
            "populated",
            "other_parent",
            "vars_only",
            "cycle_a",
            "cycle_b",
        ] {
            inv.add_group(group);
        }

        inv.add_child(ALL, "empty_leaf");
        inv.add_child(ALL, "empty_branch");
        inv.add_child("empty_branch", "empty_leaf");
        inv.add_child(ALL, "populated");
        inv.add_child(ALL, "other_parent");
        inv.add_child("other_parent", "populated");
        inv.add_child(ALL, "vars_only");
        inv.add_child(ALL, "cycle_a");
        inv.add_child("cycle_a", "cycle_b");
        inv.add_child("cycle_b", "cycle_a");

        inv.add_host("kept");
        inv.add_host_to_group("kept", "populated");
        inv.add_host("cycle_host");
        inv.add_host_to_group("cycle_host", "cycle_b");
        inv.set_group_var("vars_only", "purpose", Value::String("export".into()));
        inv
    }

    fn available(names: &[&str]) -> HashSet<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    #[test]
    fn graph_pruning_keeps_populated_groups_and_all_ancestors() {
        let inv = test_inventory();
        let selected = available(&["kept"]);
        let out = build_list(&inv, &selected, false, GroupPruning::GraphAware);

        assert_eq!(
            out[ALL]["children"],
            serde_json::json!(["populated", "other_parent"])
        );
        assert_eq!(out["populated"]["hosts"], serde_json::json!(["kept"]));
        assert_eq!(
            out["other_parent"]["children"],
            serde_json::json!(["populated"])
        );
        for absent in [
            "empty_leaf",
            "empty_branch",
            "vars_only",
            "cycle_a",
            "cycle_b",
        ] {
            assert!(out.get(absent).is_none(), "{absent} should be pruned");
        }
    }

    #[test]
    fn graph_pruning_removes_every_group_for_an_empty_selection() {
        let inv = test_inventory();
        let selected = available(&[]);
        let out = build_list(&inv, &selected, false, GroupPruning::GraphAware);

        assert_eq!(out, serde_json::json!({"_meta": {"hostvars": {}}}));
    }

    #[test]
    fn export_retains_vars_only_groups_and_their_ancestors() {
        let inv = test_inventory();
        let selected = available(&[]);
        let out = build_list(&inv, &selected, true, GroupPruning::GraphAware);

        assert_eq!(out[ALL]["children"], serde_json::json!(["vars_only"]));
        assert_eq!(
            out["vars_only"]["vars"],
            serde_json::json!({"purpose": "export"})
        );
    }

    #[test]
    fn strict_json_preserves_dangling_references_and_empty_cycles() {
        let inv = test_inventory();
        let selected = available(&["kept"]);
        let out = build_list(&inv, &selected, false, GroupPruning::AnsibleCompatible);

        assert!(
            out[ALL]["children"]
                .as_array()
                .unwrap()
                .contains(&Value::String("empty_leaf".into()))
        );
        assert!(out.get("empty_leaf").is_none());
        assert!(out.get("cycle_a").is_some());
        assert!(out.get("cycle_b").is_some());
    }

    #[test]
    fn yaml_and_toml_use_the_same_retained_group_set() {
        let inv = test_inventory();
        let selected = available(&["kept"]);
        let yaml = build_yaml(&inv, &selected, false, GroupPruning::GraphAware);
        let toml = build_toml(&inv, &selected, false, GroupPruning::GraphAware);

        let yaml_children = yaml[ALL]["children"].as_object().unwrap();
        assert_eq!(
            yaml_children.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["populated", "other_parent"]
        );
        assert!(toml.get("populated").is_some());
        assert!(toml.get("other_parent").is_some());
        for absent in [
            "empty_leaf",
            "empty_branch",
            "vars_only",
            "cycle_a",
            "cycle_b",
        ] {
            assert!(yaml_children.get(absent).is_none());
            assert!(toml.get(absent).is_none());
        }
    }

    #[test]
    fn populated_cycles_are_retained_without_recursive_traversal() {
        let inv = test_inventory();
        let selected = available(&["cycle_host"]);
        let json = build_list(&inv, &selected, false, GroupPruning::GraphAware);
        let yaml = build_yaml(&inv, &selected, false, GroupPruning::GraphAware);
        let toml = build_toml(&inv, &selected, false, GroupPruning::GraphAware);

        assert_eq!(json[ALL]["children"], serde_json::json!(["cycle_a"]));
        assert_eq!(json["cycle_a"]["children"], serde_json::json!(["cycle_b"]));
        assert_eq!(json["cycle_b"]["children"], serde_json::json!(["cycle_a"]));
        assert!(yaml[ALL]["children"]["cycle_a"].is_object());
        assert_eq!(toml["cycle_a"]["children"], serde_json::json!(["cycle_b"]));
        assert_eq!(toml["cycle_b"]["children"], serde_json::json!(["cycle_a"]));
    }
}
