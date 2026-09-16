# ansible-inventory behavior anomalies

Notes on surprising `ansible-inventory` behaviors discovered while building `fleets` and
validating it differentially against ansible-core **2.18.9**. Each entry says whether
`fleets` replicates the behavior or intentionally diverges, with the ansible source
location where it's useful.

The categories:
- **[match]** — fleets reproduces this exactly (verified by the differential harness).
- **[diverge]** — fleets intentionally behaves differently; explained why.
- **[env]** — a gotcha about *running* ansible, not about inventory semantics.

---

## Variable precedence & resolution

### 1. `ansible_group_priority` is consumed, not stored `[match]`
When a group sets `ansible_group_priority`, ansible's `Group.set_variable`
(`inventory/group.py`) calls `set_priority(int(value))` and does **not** keep it in the
group's vars. So it never appears in resolved `_meta.hostvars`. Under `--export`,
`_get_group_variables` re-adds it **only when the priority is not the default 1**.

This is easy to get wrong: a naive implementation leaves `ansible_group_priority` in the
output. fleets strips it on ingest and re-adds it under export exactly when ansible does.

### 2. Group precedence key is `(depth, priority, name)` — not definition order `[match]`
`inventory/helpers.py::sort_groups` sorts a host's groups by `(depth, priority, name)` and
folds their vars left-to-right. Consequences people miss:
- Lower depth = lower precedence (applied first). `all` (depth 0) is always lowest.
- Same-depth ties break **alphabetically by name**, and the alphabetically-*later* name
  wins (it's combined last). Definition/file order is irrelevant.

### 3. `group_vars`/`host_vars` files outrank inventory-defined vars `[match]`
The precedence ladder (low → high), from `vars/manager.py`:
1. `all` group inventory vars
2. `group_vars/all` files
3. all other groups' inventory vars (sorted fold)
4. all other groups' `group_vars/<group>` files (sorted fold)
5. host inventory vars
6. `host_vars/<host>` files

Note steps 3 vs 4: **every** `group_vars/<g>` file outranks **every** inventory-defined
group var, regardless of group depth/priority. A var file from a low-priority group beats
an inventory-source var from a high-priority group.

### 4. `combine_vars` default is `replace`, not merge `[match]`
With the default `hash_behaviour=replace`, `combine_vars(a, b)` is `a | b` — a shallow
top-level overwrite. Nested dicts are replaced wholesale, **not** deep-merged.

---

## Output shape & format

### 5. `--export` is a variable-placement change, not a format change `[match]`
`--export` is orthogonal to `--yaml`/`--toml`. Without it, `_meta.hostvars` is fully
flattened (group vars copied onto every host). With it, hosts carry only their *own* vars
(+ `host_vars/` files) and group vars live in each group's `vars` block — the
"round-trippable" representation (`cli/inventory.py::_get_host_variables`).

### 6. Default `--list` JSON omits group `vars` entirely `[match]`
In plain `--list` JSON, no group emits a `vars` block — variables exist *only* flattened
in `_meta.hostvars`. Group `vars` blocks appear **only under `--export`**. (It's tempting
to assume `--list` shows group vars inline; it does not.)

### 7. The three output formats are genuinely different trees `[match]`
Not just different serializers — different structures (`cli/inventory.py`):
- **JSON**: flat map of groups, `children`/`hosts` as **lists**, plus `_meta.hostvars`.
- **YAML**: a single nested `all:` tree; `children`/`hosts` as **maps**; host vars inlined.
- **TOML**: flat map of groups; `children` as a **list**; `all` lists no children (so it's
  usually pruned); `ungrouped` omitted as a child unless it actually has hosts.

### 8. YAML/TOML inline host vars only at the *first* occurrence `[match]`
A host in multiple groups gets its vars rendered under the **first** group encountered
during traversal (a `seen_hosts` guard); later occurrences render as an empty map. So the
*traversal order* (which follows source/insertion order) decides which group carries the
vars. This is why fleets parses with `serde_json`'s `preserve_order` — sorting keys on
ingest would change which group "owns" a host's vars and diverge from ansible.

### 9. Empty groups and child references are pruned recursively `[diverge]`
By default, fleets retains groups with visible hosts or exported vars and all of their
ancestors. Other groups and their `children` references are removed, including nested empty
branches and empty cycles. The inventory model is unchanged, so pruned groups still
participate in precedence before rendering. Ansible prunes only empty group bodies and can
leave their names dangling in parent `children` lists; `--strict-compatibility` or
`FLEETS_STRICT_COMPATIBILITY=true` restores that exact behavior.

### 10. TOML output omits `null` values `[match]`
TOML has no null. ansible's `toml_dumps` silently drops keys whose value is `None`, while
JSON/YAML keep them. fleets strips nulls recursively only on the TOML path.

### 11. ansible cannot TOML-serialize cyclic inventories `[env]`
`--toml` on a mutually-referential group graph (a → b → a) makes ansible exit non-zero
(recursion). This is an ansible limitation, not a spec. fleets handles it without looping;
the differential harness simply skips any case where ansible itself errors.

---

## Source parsing

### 12. Group-name sanitization rewrites invalid characters `[match]`
ansible applies the regex `^[\d\W]|[^\w]` → `_` to every group name
(`inventory/group.py::to_safe_group_name`, with `TRANSFORM_INVALID_GROUP_CHARS=silently`):
a leading digit or any non-word character becomes `_`. So `web-servers` → `web_servers`,
`web.east` → `web_east`, `1web` → `_web`. A non-leading digit is fine (`web1` stays).

### 13. INI values are coerced via Python's `ast.literal_eval` `[match]`
`ini.py::_parse_value` runs `ast.literal_eval` and falls back to a string on failure.
Notable consequences (Python literal rules, not YAML/JSON):
- `x=1` → int, `x=1.0` → float, `x=True`/`x=False` → bool (**capitalized**), `x=None` → null.
- `x=01` or `x=007` → **string** `"01"`/`"007"` (leading-zero ints are a Python SyntaxError).
- `x=[1, 'two', 3.0]` → list; `x={'a': 1}` → dict; single-quoted strings are valid.
- `x=foo` → string `"foo"`; `x=8.8.8.8` → string (not a valid number).

### 14. `host:port` in INI becomes an `ansible_port` integer var `[match]`
`web01:2222` yields host `web01` with `ansible_port: 2222` (an int). The colon for the port
is distinguished from colons inside `[range]` brackets.

### 15. Host-range expansion has zero-padding and equal-length rules `[match]`
`expand_hostname_range` (`inventory/__init__.py`): `web[01:50]` zero-pads to the width of
the *begin* token; if begin starts with `0` and is longer than one char, begin and end
**must be equal length** or it errors. Also supports a step (`[01:10:2]`), alphabetic ranges
(`[a:z]`), multiple ranges per host, and empty-begin defaulting to `0`.

### 16. JSON files use the *YAML* structured schema `[match]`
ansible's YAML inventory plugin accepts `.json` (YAML is a JSON superset). There is no
separate "static JSON inventory" plugin — a `.json` file is parsed with the structured
group/hosts/vars schema, not as dynamic-script output.

### 17. TOML's `children` is a list of names, not a nested map `[match]`
Unlike YAML (nested `children:` map), the TOML schema lists `children = ["a", "b"]` and the
subgroups are defined as separate top-level tables.

### 18. A group is a child of `all` only if it has no other parent `[match]`
`reconcile_inventory` (`inventory/data.py:116`) adds a group to `all` only when it has no
ancestors. This matters for TOML/INI where subgroups are declared as their own
sections/tables *and* referenced in a parent's `children` — they must **not** also become
children of `all`.

---

## Directory inventories

### 19. Directory inventories silently ignore many files — including `.ini`/`.cfg` `[match]`
`InventoryManager`'s `IGNORED` regex skips: dotfiles, the `group_vars`/`host_vars`/
`vars_plugins` subdir names, and the extensions
`.pyc .pyo .swp .bak ~ .rpm .md .txt .rst .orig .ini .cfg .retry`. So an `.ini` file dropped
inside a directory inventory is **silently skipped** (even though `.ini` works fine as a
*direct* `-i` source). Surprising, and a common "why isn't my host showing up" cause.

### 20. `group_vars/<name>` can be a file *or* a directory of files `[match]`
`DataLoader.find_vars_files` checks the extension-less name first (so a `<name>/` directory
is found before a `<name>.yml` file), then `.yml`/`.yaml`/`.json`. A matching directory has
all its files merged (sorted; hidden/backup skipped; recursing into extension-less subdirs).

---

## Dynamic (script) inventories

### 21. `_meta.hostvars` suppresses the per-host fallback `[match]`
The script is run with `--list`. If the output contains `_meta.hostvars`, ansible uses it
and does **not** call the script again. If `_meta` is absent, ansible runs the script once
per host with `--host <name>` (the legacy contract). fleets implements both paths.

### 22. Group values have three accepted shapes `[match]`
In `--list` output (`script.py::_parse_group`): a group value may be a full
`{hosts, vars, children}` object, a **bare list of hostnames** (shorthand for `{hosts: …}`),
or — if it's an object lacking all three known keys — "simplified syntax" meaning a single
host named after the group, carrying that object as its vars.

---

## Environment / harness gotchas `[env]`

### 23. `LC_ALL=C` makes ansible refuse to run
ansible requires a UTF-8 locale: `ERROR: Ansible requires the locale encoding to be UTF-8;
Detected None.` The differential harness uses `LC_ALL=en_US.UTF-8`.

### 24. Inventory plugin order matters — `auto` must precede `ini`
The INI plugin is permissive enough to "claim" a `.json`/`.yaml` file and turn stray lines
(e.g. a bare `{`) into phantom hosts. With ansible's **default** enabled order
(`host_list,script,auto,yaml,ini,toml`), `auto` dispatches by extension first and this
doesn't happen. Reordering `ini` ahead of `auto`/`yaml` (a tempting "hermetic" override)
silently corrupts results. Also note: multiple plugins can each contribute to the *same*
source, so a wrongly-ordered `ini` can add phantom hosts *in addition to* the correct YAML
parse.

---

## Intentional fleets divergences `[diverge]`

### 25. Same-name host and group under `--export`
When a host and a group share a name (e.g. both `foo`), ansible under `--export` merges the
host's vars into the group's `vars` block and leaves `_meta.hostvars` **empty** — an
apparent ansible quirk/bug. fleets keeps the two namespaces distinct. Non-export modes
match ansible exactly; only `--export` differs (quarantined in the test harness).

### 26. Leniency on malformed INI
An INI `[group:vars]` section for a group never declared with `[group]`/`[group:children]`
is invalid: ansible **fails to parse the whole file**, warns, and yields an empty inventory
(exit 0, not a hard error). fleets is more lenient and accepts such input. Replicating
ansible's "all plugins failed → empty inventory" soft-failure for malformed sources was
judged not worth the complexity for a `mostly-compatible` tool.

### 27. `--graph` is not implemented
Designed-for but deferred. The data model supports it; the CLI does not yet expose it.

---

## Constructed plugin (`ansible.builtin.constructed`)

fleets implements a **minimal subset** of the `constructed` post-processing plugin
(`plugins/inventory/constructed.py` + the `Constructable` mixin): `compose`, `keyed_groups`,
and `groups`, driven by a small expression evaluator (`src/expr.rs`). Full Jinja2 is not
implemented. The behaviors below were verified differentially against ansible-core 2.18.9.

### 28. Constructed is an incremental post-processing pass `[match]`
A `plugin: ansible.builtin.constructed` source runs **at the position it sorts to** and sees
only the hosts/groups loaded *before* it. In a directory inventory, a constructed file in the
middle keys only the earlier hosts; hosts loaded by later files are untouched (they fall into
`ungrouped`). This is why the convention is to name it `~constructed.yml` — see #29. fleets
runs the pass inline in `parse_source`, so it gets this incremental visibility for free (for
both directory and multi-`-i` inventories). A standalone `-i ~constructed.yml` with no prior
hosts is a no-op.

### 29. Directory file order is codepoint sort, not locale `[match]`
ansible orders directory entries with Python's `sorted()` (Unicode codepoint order), **not**
locale collation. `~` is `0x7E`, *after* `z` (`0x7A`), so `~constructed.yml` sorts **last** and
runs after every lettered file. (Note: macOS `ls` uses locale collation and lists `~`-files
*first* — misleading. The behavior depends on Python's sort.) Rust's default `str` ordering is
also by Unicode scalar value, so fleets matches without special-casing.

### 30. Constructed sees only inventory-inline vars, not `*_vars/` files `[match]`
The pass runs during parsing, before the `host_group_vars` plugin layers in `group_vars/`/
`host_vars/` *files*. So a `keyed_groups`/`groups` expression keying off a var that lives only
in `group_vars/<g>.yml` produces **no** group; the same var written inline does. It *does* see
inline group vars folded onto the host (normal precedence). fleets mirrors this by resolving
with `vars::resolve_host_inline_vars` (the precedence fold minus `file_vars`), which is natural
because fleets also loads those files only after all parsing.

### 31. Unsupported expressions error loudly instead of guessing `[diverge]`
The supported grammar covers what real `compose`/`keyed_groups`/`groups` configs use:
variable refs (incl. dotted paths), `'str'`/int/float/bool/`none` literals, postfix
subscripting and slicing (`x[i]`, `x[a:b:c]`, Python semantics), the arithmetic operators
`+ - * / // % **`, string concatenation `~`, unary `-`, the filters
`lower upper string int float bool default(x[,bool]) d replace regex_replace trim capitalize
length count first last join abs ternary(true,false[,none])`, the tests `is [not] defined` /
`is undefined`, comparisons
(`== != < <= > >=`), `in`/`not in`, `and`/`or`/`not`, and the conditional (ternary)
expression `body if cond [else orelse]` (right-associative, so nested-ternary buckets parse;
a false condition with no `else` is Undefined). Anything outside it (e.g. `map`,
`selectattr`, `match`, dict/list literals, an unknown filter) makes fleets **exit non-zero
with the offending expression named**, rather than silently producing a wrong group/var.
ansible, with the default `strict: false`, would evaluate the full Jinja2 — so these inputs
are a deliberate divergence (fail-loud over silently-wrong).

Three failure modes are distinguished internally: *unsupported syntax* always errors (even
under `strict: false`); a *runtime failure* (type mismatch in `+`, slicing a number, a bad
regex) and an *undefined variable* are both swallowed under `strict: false` — an undefined
`keyed_groups` key skips the host, an undefined/failed `groups` condition is falsey, and a
failed `compose` expression leaves the var unset (see §32). `ternary` evaluates only the
selected branch (so a nested-ternary bucket and an undefined *untaken* branch never fail the
host); an undefined operand takes the false branch (`bool(Undefined)` is false), distinct from
a `none` operand, which takes the optional third `none_val` branch when one is supplied.
`regex_replace` is best-effort:
Python `\1`/`\g<name>` backreferences are translated to the Rust `regex` crate's `${1}`/
`${name}`, but exotic Python-`re` constructs may still diverge.

### 32. `compose` semantics: ordering, snapshot isolation, and type coercion `[match]`
fleets implements the constructed plugin's `compose` (new host vars), matching ansible:

- **Ordering.** Per host, `compose` runs first; its results are written as host vars (highest
  precedence), then `groups` and `keyed_groups` run against the post-compose view. So a
  `keyed_groups` key can bucket on a var that `compose` just synthesized.
- **Snapshot isolation.** Every `compose` expression is evaluated against the *same*
  pre-compose variable snapshot — entries do **not** observe each other's results. Mutually
  defaulting pairs like `region: region | default(zone[:-1])` / `zone: zone | default(region
  + "a")` therefore each see only the original vars. (Verified differentially.)
- **`strict` (default false).** An expression that fails (undefined var, runtime/type error)
  silently leaves its var unset; with `strict: true` it is a hard error.
- **Type coercion.** ansible's default (non-`jinja2_native`) templating preserves a value's
  native type only for a template that is *textually* a bare variable (`{{ var }}`). Any
  *computed* expression — including `(var)`, `var.attr`, `var[i]`, or anything with a filter
  or operator — is rendered to text and lightly re-typed: a number becomes its **string**
  form (`idx + 1` → `"6"`), `None`/null becomes `""`, and booleans / lists / dicts / strings
  pass through. fleets reproduces this in `expr::eval_compose`. (Every real-world compose
  expression in our inventories yields a string, so the coercion is invisible there; it
  matters only for numeric/None results.)
