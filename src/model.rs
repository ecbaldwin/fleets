//! Core inventory data model: hosts, groups, and the container that owns them.
//!
//! Mirrors ansible-core's `inventory/data.py`, `group.py`, `host.py`. Group depth and
//! `ansible_group_priority` are tracked here because they drive variable precedence
//! (see [`crate::vars`]).

use indexmap::IndexMap;
use serde_json::{Map, Value};

use crate::sanitize::to_safe_group_name;

/// Special group always present and always the root of the group tree (depth 0).
pub const ALL: &str = "all";
/// Special group holding every host that belongs to no other group.
pub const UNGROUPED: &str = "ungrouped";

#[derive(Debug, Clone)]
pub struct Host {
    pub name: String,
    pub vars: Map<String, Value>,
    /// Vars from adjacent `host_vars/<name>` files. Higher precedence than `vars`.
    pub file_vars: Map<String, Value>,
    /// Groups this host was *explicitly* added to, excluding `all`/`ungrouped`.
    /// Used to decide `ungrouped` membership; ancestry is computed on demand.
    pub groups: Vec<String>,
}

impl Host {
    fn new(name: impl Into<String>) -> Self {
        Host {
            name: name.into(),
            vars: Map::new(),
            file_vars: Map::new(),
            groups: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Group {
    pub name: String,
    /// Distance from `all`. `all` = 0. Drives precedence ordering.
    pub depth: u32,
    /// `ansible_group_priority`, default 1. Drives precedence ordering.
    pub priority: i64,
    pub hosts: Vec<String>,
    pub children: Vec<String>,
    pub parents: Vec<String>,
    pub vars: Map<String, Value>,
    /// Vars from adjacent `group_vars/<name>` files. Higher precedence than `vars`.
    pub file_vars: Map<String, Value>,
}

impl Group {
    fn new(name: impl Into<String>) -> Self {
        Group {
            name: name.into(),
            depth: 0,
            priority: 1,
            hosts: Vec::new(),
            children: Vec::new(),
            parents: Vec::new(),
            vars: Map::new(),
            file_vars: Map::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct InventoryData {
    pub hosts: IndexMap<String, Host>,
    pub groups: IndexMap<String, Group>,
}

impl Default for InventoryData {
    fn default() -> Self {
        Self::new()
    }
}

impl InventoryData {
    /// A fresh inventory containing only the special groups `all` and `ungrouped`,
    /// with `ungrouped` already a child of `all`.
    pub fn new() -> Self {
        let mut inv = InventoryData {
            hosts: IndexMap::new(),
            groups: IndexMap::new(),
        };
        inv.groups.insert(ALL.to_string(), Group::new(ALL));
        inv.add_group(UNGROUPED);
        inv.add_child(ALL, UNGROUPED);
        inv
    }

    /// Ensure a group exists (creating it as a child-less, host-less group parented to
    /// nothing yet). Names are sanitized to ansible's safe-group-name form. Returns the
    /// sanitized name actually used as the key.
    pub fn add_group(&mut self, name: &str) -> String {
        let safe = to_safe_group_name(name);
        if !self.groups.contains_key(&safe) {
            self.groups.insert(safe.clone(), Group::new(safe.clone()));
        }
        safe
    }

    /// Ensure a host exists. Returns nothing; use [`InventoryData::host_mut`] to mutate.
    pub fn add_host(&mut self, name: &str) {
        if !self.hosts.contains_key(name) {
            self.hosts.insert(name.to_string(), Host::new(name));
        }
    }

    pub fn host_mut(&mut self, name: &str) -> &mut Host {
        self.hosts
            .get_mut(name)
            .expect("host must be added before mutation")
    }

    pub fn group_mut(&mut self, name: &str) -> &mut Group {
        self.groups
            .get_mut(name)
            .expect("group must be added before mutation")
    }

    /// Add `host` to `group` (both must already exist). Records the membership on both
    /// sides; `all`/`ungrouped` memberships are not recorded on the host's explicit list.
    pub fn add_host_to_group(&mut self, host: &str, group: &str) {
        let g = self.group_mut(group);
        if !g.hosts.iter().any(|h| h == host) {
            g.hosts.push(host.to_string());
        }
        if group != ALL && group != UNGROUPED {
            let h = self.host_mut(host);
            if !h.groups.iter().any(|x| x == group) {
                h.groups.push(group.to_string());
            }
        }
    }

    /// Set a single group variable. `ansible_group_priority` is consumed into the group's
    /// priority and not stored as a normal var (matching ansible's `Group.set_variable`).
    pub fn set_group_var(&mut self, group: &str, key: &str, value: Value) {
        if key == "ansible_group_priority" {
            if let Some(p) = value.as_i64() {
                self.group_mut(group).priority = p;
            }
            return;
        }
        self.group_mut(group).vars.insert(key.to_string(), value);
    }

    /// Set a single host variable.
    pub fn set_host_var(&mut self, host: &str, key: &str, value: Value) {
        self.host_mut(host).vars.insert(key.to_string(), value);
    }

    /// Link `child` under `parent` and update depths of `child` and its descendants.
    /// Both groups must already exist (use the sanitized names).
    pub fn add_child(&mut self, parent: &str, child: &str) {
        if parent == child {
            // ansible guards against self-membership; ignore it silently.
            return;
        }
        {
            let p = self.group_mut(parent);
            if !p.children.iter().any(|c| c == child) {
                p.children.push(child.to_string());
            }
        }
        {
            let c = self.group_mut(child);
            if !c.parents.iter().any(|x| x == parent) {
                c.parents.push(parent.to_string());
            }
        }
        self.recalc_depth(parent, child);
    }

    /// Set `child.depth = max(parent.depth + 1, child.depth)` and propagate to descendants,
    /// guarding against cycles. Mirrors `Group._check_children_depth`.
    fn recalc_depth(&mut self, parent: &str, child: &str) {
        let parent_depth = self.groups[parent].depth;
        let new_depth = parent_depth + 1;
        if self.groups[child].depth >= new_depth {
            return;
        }
        self.group_mut(child).depth = new_depth;

        // Propagate downward. `seen` prevents infinite loops on cyclic child graphs.
        let mut seen = std::collections::HashSet::new();
        let mut stack = vec![child.to_string()];
        while let Some(g) = stack.pop() {
            if !seen.insert(g.clone()) {
                continue;
            }
            let depth = self.groups[&g].depth;
            let children = self.groups[&g].children.clone();
            for c in children {
                let want = depth + 1;
                if self.groups[&c].depth < want {
                    self.group_mut(&c).depth = want;
                    stack.push(c);
                }
            }
        }
    }

    /// After all sources are parsed, make every group that has no explicit parent a child
    /// of `all` (mirrors `reconcile_inventory`'s "groups inherit from all" rule). `all`
    /// and `ungrouped` are left as-is.
    pub fn reconcile_all_children(&mut self) {
        let orphans: Vec<String> = self
            .groups
            .values()
            .filter(|g| g.name != ALL && g.parents.is_empty())
            .map(|g| g.name.clone())
            .collect();
        for g in orphans {
            self.add_child(ALL, &g);
        }
    }

    /// After all sources are parsed, place every host that belongs to no explicit group
    /// into `ungrouped`. Idempotent.
    pub fn reconcile_ungrouped(&mut self) {
        let orphans: Vec<String> = self
            .hosts
            .values()
            .filter(|h| h.groups.is_empty())
            .map(|h| h.name.clone())
            .collect();
        for h in orphans {
            let g = self.group_mut(UNGROUPED);
            if !g.hosts.contains(&h) {
                g.hosts.push(h);
            }
        }
    }

    /// All host names in `group` and every descendant (child) group, deduplicated.
    /// Mirrors ansible's `Group.get_hosts()`, which is recursive.
    pub fn group_all_hosts(&self, group: &str) -> Vec<String> {
        let mut seen_groups = std::collections::HashSet::new();
        let mut seen_hosts = std::collections::HashSet::new();
        let mut result = Vec::new();
        let mut stack = vec![group.to_string()];
        while let Some(g) = stack.pop() {
            if !seen_groups.insert(g.clone()) {
                continue;
            }
            if let Some(grp) = self.groups.get(&g) {
                for h in &grp.hosts {
                    if seen_hosts.insert(h.clone()) {
                        result.push(h.clone());
                    }
                }
                for c in &grp.children {
                    stack.push(c.clone());
                }
            }
        }
        result
    }

    /// All groups a host belongs to, transitively (direct groups + their ancestors),
    /// always including `all`. Used for variable resolution.
    pub fn host_groups(&self, host: &str) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        let mut result = Vec::new();
        let mut stack: Vec<String> = Vec::new();

        // Seed with the host's direct memberships plus `all` and (if applicable) `ungrouped`.
        if let Some(h) = self.hosts.get(host) {
            if h.groups.is_empty() {
                stack.push(UNGROUPED.to_string());
            }
            for g in &h.groups {
                stack.push(g.clone());
            }
        }
        stack.push(ALL.to_string());

        while let Some(g) = stack.pop() {
            if !seen.insert(g.clone()) {
                continue;
            }
            result.push(g.clone());
            if let Some(grp) = self.groups.get(&g) {
                for p in &grp.parents {
                    stack.push(p.clone());
                }
            }
        }
        result
    }
}
