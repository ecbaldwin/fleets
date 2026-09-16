# Environment variables

ansible-inventory's behavior is shaped by a long list of `ANSIBLE_*` environment variables
(each backing an `ansible.cfg` key). fleets is a drop-in for the *output*, so this page
records which of those variables fleets honors, which it does not, and which only matter to
the differential test harness.

Defaults below are quoted from ansible-core 2.18.9 (`ansible-config dump`).

## Honored by fleets

| Variable | cfg key | Default | fleets behavior |
|---|---|---|---|
| `ANSIBLE_INVENTORY` | `DEFAULT_HOST_LIST` | `/etc/ansible/hosts` | When no `-i/--inventory` is given, used as the source(s), split on `,` (trimmed, empties dropped). An explicit `-i` always overrides it. **Non-goal:** the further fallback to `/etc/ansible/hosts` when the env is also unset — fleets errors instead of scanning a system path. Resolved in `cli::resolve_sources`. |
| `ANSIBLE_INVENTORY_ENABLED` | `INVENTORY_ENABLED` | `host_list, script, auto, yaml, ini, toml` | Restricts which source *kinds* fleets will parse; when set it *replaces* the default list (only the named kinds are enabled), as in ansible. Disabling `script` leaves executable sources unparsed (not executed); disabling `auto` turns off the `constructed` post-processing pass (the file is then parsed as an ordinary document). A disabled kind is **skipped** with a warning, not a hard error — matching ansible's "unparsed source" default. Strict plugin *ordering* for ambiguous extensionless files is **not** reproduced (fleets dispatches by extension; an extensionless file valid as both INI and YAML resolves to YAML regardless of list order). `host_list` (inline `-i h1,h2,` comma strings) is unsupported and ignored. Modeled in `enabled::EnabledKinds`. |
| `FLEETS_STRICT_COMPATIBILITY` | — | `false` | Restores ansible's shallow group pruning and dangling `children` references. Accepts case-insensitive `true`/`false` and `1`/`0`; any other value is an error. A true value or `--strict-compatibility` enables the mode. |
| `XDG_CONFIG_HOME` / `HOME` | — | — | Locate the tracing config at `$XDG_CONFIG_HOME/fleets/config.toml` (falls back to `$HOME/.config/...`). fleets-specific, not an ansible variable. See [`tracing.md`](tracing.md). |

## Not honored (semantic — a known compatibility gap)

These change ansible's resolved graph but fleets ignores them; behavior is fixed to the
ansible default shown:

| Variable | cfg key | Default | fleets is fixed to |
|---|---|---|---|
| `ANSIBLE_FORCE_VALID_GROUP_NAMES` | `TRANSFORM_INVALID_GROUP_CHARS` | `always` (the user's `~/.ansible.cfg` sets `silently`) | `silently` — always sanitizes `[^A-Za-z0-9_]` → `_`. `never`/`ignore` (which preserve invalid characters) are not selectable. |
| `ANSIBLE_HASH_BEHAVIOUR` | `DEFAULT_HASH_BEHAVIOUR` | `replace` | `replace` — `combine_vars` is shallow overwrite. The non-default `merge` is not implemented. |
| `ANSIBLE_INVENTORY_IGNORE` | `INVENTORY_IGNORE_EXTS` | `.pyc .pyo .swp .bak ~ .rpm .md .txt .rst .orig .ini .cfg .retry` | A built-in ignore list in `parse::is_ignored_dir_entry`; not configurable via env. (Note: ansible's default ignores `.ini`/`.cfg` inside directory inventories — fleets matches this.) |
| `ANSIBLE_INVENTORY_IGNORE_REGEX` | `INVENTORY_IGNORE_PATTERNS` | `[]` | No regex-based ignore overrides. |
| `ANSIBLE_JINJA2_NATIVE` | `DEFAULT_JINJA2_NATIVE` | `False` | `False` — `constructed` `compose`/`keyed_groups` results follow non-native (text-then-retyped) coercion. See [`anomalies.md`](anomalies.md) §32. |
| `ANSIBLE_INVENTORY_UNPARSED_FAILED` | `INVENTORY_UNPARSED_IS_FAILED` | `False` | Affects exit code on unparseable sources; fleets has its own error model and does not toggle on this. |
| `ANSIBLE_INVENTORY_EXPORT` | `INVENTORY_EXPORT` | `False` | The `--export` placement is controlled by the flag only, not defaulted from env. |

## Not honored (harness hygiene — cosmetic only)

These do not change the resolved graph but make ansible non-deterministic or noisy. The
differential harness (`tests/differential.rs`) pins them so the two tools compare cleanly;
fleets itself ignores them.

| Variable | Purpose | Harness setting |
|---|---|---|
| `LC_ALL` / `LANG` | Locale drives `sorted()` collation — directory file order and group sort. | `en_US.UTF-8` (codepoint-stable: `~` sorts after `z`) |
| `ANSIBLE_NOCOLOR` / `NO_COLOR` | Strip ANSI escapes from stderr. | `1` |
| `ANSIBLE_CONFIG` | Pin/skip `ansible.cfg` discovery so a stray `~/.ansible.cfg` can't leak settings. | pinned in harness |
| `ANSIBLE_DEPRECATION_WARNINGS`, `ANSIBLE_LOCALHOST_WARNING`, `ANSIBLE_INVENTORY_UNPARSED_WARNING` | Quiet stderr noise. | quieted |

## Note: dynamic (script) sources

Script inventories inherit the full process environment when fleets execs them, so any
variable — `ANSIBLE_*` or arbitrary app config — can affect a dynamic source's output
transitively. That is the script's behavior, outside fleets' control.
