# Tracing

When compiled with the optional `telemetry` Cargo feature, fleets can emit
[OpenTelemetry](https://opentelemetry.io/) spans for inventory processing and export them to an
OTLP/HTTP collector such as [Jaeger](https://www.jaegertracing.io/). This is useful for
understanding *where time goes* on large directory inventories or slow dynamic scripts.

The feature and runtime export are both **off by default**. Build with
`cargo build --release --features telemetry`; then enable runtime export with the configuration
below.

## Enable it

Config lives at `$XDG_CONFIG_HOME/fleets/config.toml` (default `~/.config/fleets/config.toml`):

```toml
[tracing]
enabled = true
# OTLP/HTTP base URL of the collector ("/v1/traces" is appended automatically).
endpoint = "http://localhost:4318"
service_name = "fleets"
```

- A missing config file → tracing disabled.
- A malformed config → tracing disabled, with a one-line warning on stderr (the inventory run
  itself is never affected).
- Enabled but the collector is unreachable → spans are dropped; fleets does not block or fail.

## Run Jaeger locally

```sh
docker run --rm -p 16686:16686 -p 4318:4318 jaegertracing/all-in-one:latest
```

Then run any fleets command and open the UI at <http://localhost:16686>, service `fleets`.

## What is traced (high-level)

```
load_inventory                     # the whole load (attr: sources = N)
├─ parse_yaml      (path=…)        # one span per top-level -i source, named by kind:
├─ parse_ini       (path=…)        #   parse_yaml / parse_toml / parse_ini / parse_script
├─ parse_script    (path=…)
└─ parse_directory (path=…)        # a directory inventory…
   ├─ run_script   (path=…)  ┐     # …dynamic scripts' external queries run CONCURRENTLY
   ├─ run_script   (path=…)  ┘     #   (these two spans overlap in time)
   ├─ parse_yaml   (path=…)        # then every sub-source is ingested in sorted order:
   ├─ parse_script (path=…)        #   prefetched scripts ingest here (attr: parallel = true)
   └─ parse_yaml   (path=…)        # a constructed source is parsed as yaml first…
      └─ constructed               # …then its synthesis pass (attrs: hosts, keyed_groups, groups)
```

This is deliberately coarse to start — one span per source processed, directory sub-sources
nested under the directory, and the `constructed` synthesis pass as its own span. Finer spans
(per-host var resolution, per-keyed-group, output rendering) can be added later if needed.

When a directory holds more than one dynamic-script source, fleets runs their `--list`
queries concurrently (a bounded standard-thread worker pool) and only the slow external query overlaps — the
results are held in memory and ingested **sequentially in sorted order**, so the `run_script`
spans overlap while ingest stays ordered (and constructed sees the correct host subset). For a
directory of two slow scripts, wall time drops from roughly the sum of the queries to roughly
the longest single one.

## Implementation notes

- Transport is OTLP/HTTP with the blocking reqwest client, so fleets needs no async runtime.
- Spans are batched and flushed on exit (`telemetry::Guard::shutdown`, called from `main`
  before `process::exit`, which would otherwise skip the flush).
