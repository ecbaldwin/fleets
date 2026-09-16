# fleets

A fast, high-compatibility replacement for `ansible-inventory`, written in Rust. It supports
the most common static, directory, script, and constructed inventory workflows while making
documented choices where exact Ansible behavior is undesirable.

Fleets targets Linux and macOS controllers. It is an independent project and is not affiliated
with or endorsed by Red Hat or the Ansible project. Ansible is a trademark of Red Hat, Inc.

## Install

```sh
cargo install fleets
```

OpenTelemetry support is optional because it adds a substantial dependency tree:

```sh
cargo install fleets --features telemetry
```

To build from a checkout, run `cargo build --release`; the binary is written to
`target/release/fleets`.

## Usage

```sh
fleets --list  -i <source>            # full inventory as JSON (the dynamic-inventory contract)
fleets --host <name> -i <source>      # one host's resolved variables
fleets --list -y     -i <source>      # YAML output
fleets --list --toml -i <source>      # TOML output
fleets --list --export -i <source>    # round-trippable layout (vars on groups/hosts, not flattened)
fleets --list -l 'web*:&staged:!down' -i <source>   # limit and prune to matching groups
fleets --list --strict-compatibility -i <source>   # exact ansible output shape
fleets -i a.yml -i b.ini -i dir/      # multiple sources, merged
```

Flags mirror `ansible-inventory`: `--list`, `--host`, `-i/--inventory` (repeatable),
`-l/--limit`, `-y/--yaml`, `--toml`, `--export`. Fleets also provides
`--strict-compatibility` to retain ansible's exact group-reference shape. (`--graph` is not
yet implemented.)

By default, list output prunes groups that have no visible hosts, relevant descendants, or
exported variables. This also removes dangling names from `children`. Set
`FLEETS_STRICT_COMPATIBILITY=true` or pass `--strict-compatibility` to preserve ansible's
shallow pruning instead.

fleets honors `ANSIBLE_INVENTORY` (default source when no `-i` is given) and
`ANSIBLE_INVENTORY_ENABLED` (which source kinds are enabled); most other `ANSIBLE_*`
variables are fixed to their ansible defaults. See [`docs/environment.md`](docs/environment.md)
for the full matrix of which are honored, fixed, or only pinned in the test harness.

## Supported inventory sources

| Source | Notes |
|---|---|
| **YAML / JSON** | `.yaml`/`.yml`/`.json` (JSON rides the YAML schema, as in ansible) |
| **TOML** | ansible's `toml` plugin schema (`children` as a list of names) |
| **INI** | `[group]`, `[group:children]`, `[group:vars]`, inline `key=value`, `host:port`, host ranges, `ast.literal_eval` value coercion |
| **Directory** | merges all non-ignored files (alphabetical, recursive) plus adjacent `group_vars/` and `host_vars/` |
| **Dynamic script** | executable run with `--list`; honors `_meta.hostvars`, falls back to `--host <name>` |
| **Constructed** | `plugin: ansible.builtin.constructed` — `compose` + `keyed_groups` + `groups` over a documented expression subset (runs as an incremental post-processing pass; see below) |

Variable precedence follows ansible faithfully: `all` → `group_vars/all` → other groups
(ordered by `(depth, ansible_group_priority, name)`) → `group_vars/<g>` files → host vars →
`host_vars/<h>` files. Host-range expansion, group-name sanitization, and Python-literal
value coercion all match ansible-core.

## Testing

Strict compatibility is verified against ansible-core 2.18.9 by a **differential test harness**
(`tests/differential.rs`) that runs both `fleets --strict-compatibility` and the real
`ansible-inventory` on every fixture and compares output semantically (order-independent)
across `--list`/`--host`, all three formats, `--export`, and a matrix of `--limit` patterns.

```sh
cargo test

# Required-oracle mode used in CI:
FLEETS_REQUIRE_ANSIBLE=1 cargo test --test differential
```

Without `ansible-inventory` on `PATH`, ordinary local test runs skip the differential suite.
Required-oracle mode fails instead. Known cases that ansible itself cannot serialize are
explicitly allowlisted and reported separately rather than counted as passing comparisons.

### Known intentional divergences
- **Graph-aware group pruning by default**: fleets removes child references to groups that
  contribute no visible hosts, relevant descendants, or exported variables; ansible can
  leave those names dangling. Use `--strict-compatibility` to restore ansible's shape.
- **Same-name host and group under `--export`**: ansible merges the host's vars into the
  group and empties `_meta`; fleets keeps them distinct. (Quarantined in the harness.)
- **Malformed INI**: ansible soft-fails the whole source to an empty inventory; fleets is
  more lenient on some malformed input (e.g. `[g:vars]` for an undeclared group).
- **Constructed expression subset**: fleets implements `compose`, `keyed_groups`, and
  `groups` over a documented expression grammar (arithmetic, `~`, slicing, and a curated
  filter set incl. `regex_replace`), and **errors loudly** on syntax outside it (e.g.
  `map`/`selectattr`, dict literals) instead of silently producing wrong groups/vars.
  `regex_replace` backreference translation is best-effort, so exotic Python-`re` patterns
  may diverge. See `docs/anomalies.md` §28–32 for the supported grammar and the verified
  compose ordering, snapshot-isolation, and type-coercion semantics.

## Tracing

When built with `--features telemetry`, fleets can export OpenTelemetry spans for inventory
processing to an OTLP/HTTP collector such as Jaeger. Export remains off at runtime unless
enabled via `~/.config/fleets/config.toml`:

```toml
[tracing]
enabled = true
endpoint = "http://localhost:4318"
service_name = "fleets"
```

You get a `load_inventory` root span with one child span per source (named by kind:
`parse_yaml`/`parse_toml`/`parse_ini`/`parse_script`/`parse_directory`), directory sub-sources
nested under their directory, and a `constructed` span for the constructed-plugin synthesis
pass. See [`docs/tracing.md`](docs/tracing.md) for details.

## Benchmark

```sh
cargo build --release
python3 bench/benchmark.py            # 5000 hosts by default
```

On one Apple Silicon development machine, the checked 5000-host benchmark measured 29.8 ms
for fleets and 1.55 s for ansible-core 2.18.9 (51.9× median speedup). Results vary by machine;
the script validates equivalent successful output before recording timings.

## Security

Treat inventory sources as trusted input. Executable inventory sources are run directly and
inherit the fleets process environment; fleets does not sandbox them. See
[`SECURITY.md`](SECURITY.md) for reporting vulnerabilities.

## Rust API

Fleets is CLI-first. The small API re-exported from the crate root is experimental during the
`0.1.x` series and may change in `0.2.0`.

## License

Licensed under the [MIT License](LICENSE).
