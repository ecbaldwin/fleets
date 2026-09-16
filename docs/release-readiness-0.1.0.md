# Release readiness: fleets 0.1.0

Review date: 2026-09-16

## Outcome

The local tree is prepared for an MIT-licensed 0.1.0 source release and for packaging in the
Cargo ecosystem. A separate fresh release repository contains one unsigned initial commit
on `main`; the source repository history and remotes are unchanged. Nothing was tagged,
pushed, or published as part of this work. Signed commits are not required. The intended GitHub repository already exists
and is private and empty. MIT ownership and release permission were confirmed by
Carl N Baldwin on 2026-09-16.

The supported platforms for 0.1.0 are Linux and macOS. The default Cargo feature set is lean;
OpenTelemetry is opt-in as `telemetry`. Prebuilt cargo-dist artifacts enable that feature.

## Code-review findings resolved

- Removed unmaintained or unsound dependency paths through `async-std`, `event-listener`, and
  `serde_yml`; updated `anyhow` to the fixed release.
- Replaced unbounded blocking-task spawning with a bounded standard-library worker pool.
- Prevented bare relative inventory scripts from resolving through `PATH`.
- Prevented inventory entity names from escaping adjacent `group_vars` or `host_vars`
  directories.
- Detect and reject recursive directory symlink cycles.
- Propagate directory-entry errors instead of silently dropping entries.
- Treat a closed stdout pipe as a successful CLI termination.
- Added `--version` and encoded action/output conflicts in Clap.
- Restricted the public Rust surface to a small, explicitly experimental crate-root API.
- Made telemetry dependencies optional and disabled by default.
- Made benchmark subprocess failures fatal and verify equivalent output before timing.
- Made the Ansible differential oracle required in CI, pinned its behavior, and allowlisted
  exactly eight known Ansible TOML serialization failures.
- Removed the private Gerrit `.gitreview` file from the release tree.

## Verification results

The following checks passed locally on 2026-09-16:

- `cargo fmt --all --check`
- Clippy with warnings denied for no-default-features and all-features builds
- tests with no default features and with all features
- rustdoc with warnings denied and all features
- Rust 1.85.0 checks for both feature configurations
- 403 semantic comparisons against ansible-core 2.18.9 and tomli-w 1.2.0; two expected
  Ansible failures (the harness permits eight specific known failures)
- `cargo audit` against 1,246 RustSec advisories
- `cargo deny check` for advisories, bans, licenses, and sources
- `cargo publish --dry-run --locked --allow-dirty`
- gitleaks 8.30.1 scans of both current history and the working tree
- actionlint 1.7.12 over every GitHub Actions workflow
- cargo-dist 0.32.0 planning for four targets and a native release-archive build

Fresh compilation used `DEVELOPER_DIR=/Library/Developer/CommandLineTools` because the
system-selected Xcode compiler requires license setup. No system configuration was changed.
The CI Python dependencies now live in `tests/requirements.txt`, which also supplies the
pip cache key.

The checked native cargo-dist archive contains the binary, README, changelog, and MIT license;
its binary reports `fleets 0.1.0`. The package dry run built successfully from the generated
crate archive.

The previously checked (2026-09-11) 5,000-host benchmark on an Apple Silicon development
machine measured a 29.8 ms fleets median and a 1.55 s ansible-core 2.18.9 median, a 51.9x speedup. Benchmark results are
machine-dependent.

## Known boundaries

- Dynamic inventory scripts are trusted code and execute with the process environment.
- The Rust API is experimental throughout the 0.1.x series; the CLI is the stable product
  surface for this release line.
- Compatibility exceptions are documented in `README.md` and `docs/anomalies.md`.
- Windows is not a supported or distributed 0.1.0 platform.

## External actions still required

These cannot be completed safely as local repository edits:

1. Recheck the `fleets` crate name immediately before publishing. An identified crates.io
   API lookup returned 404 on 2026-09-16; names cannot be reserved.
2. Decide whether to sign the `v0.1.0` tag before creating it. Tag signing remains optional
   and does not block committing, pushing, or running CI.
3. Push the reviewed export to the existing private `github.com/ecbaldwin/fleets` repository,
   obtain green hosted CI, and make the repository public when ready for publication.
4. Enable branch protection, Dependabot alerts, and private vulnerability reporting; verify
   GitHub Actions settings. Hosted CI and the other three release targets remain unverified.
5. Publish 0.1.0 to crates.io with a short-lived token. Then configure Trusted Publishing and
   protect the `crates-io` GitHub environment for later manual publish-workflow runs.
6. Push the `v0.1.0` tag only when ready for cargo-dist to create the GitHub release.

Use `RELEASE_CHECKLIST.md` as the operator checklist for those steps.
