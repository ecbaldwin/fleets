# Release checklist

## Before `v0.1.0`

- [x] Confirm Carl N Baldwin owns or has permission to license all code and fixtures as MIT (2026-09-16).
- [x] Review the final diff for proprietary names, paths, data, and copied third-party code.
- [x] Run the complete local validation suite documented below (2026-09-16).
- [x] Confirm `cargo package --list` contains only intended files.
- [x] Run a secret scan over the final tree and every ref that could be exported.
- [x] Review the generated crate under `target/package/fleets-0.1.0`.

## Local validation

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --locked --no-default-features
cargo test --locked --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps --all-features
FLEETS_REQUIRE_ANSIBLE=1 cargo test --locked --test differential
cargo audit
cargo deny check
cargo publish --dry-run --locked
actionlint
gitleaks git --redact
gitleaks dir --redact .
dist plan
```

## Publication boundary

Do not publish from the private repository history. Export the reviewed tree into a fresh local
Git repository, create one initial commit on `main`, and inspect its sole reachable
history before pushing to the public remote. Signed commits are not required.

The first crates.io release must be published manually with a short-lived, crate-scoped token.
After the crate exists, configure crates.io Trusted Publishing for the `Publish crate` workflow
and protect its `crates-io` GitHub environment. The release workflow publishes GitHub artifacts
from version tags; the crates.io workflow remains manual and separate.

## Open release decision

- [ ] Decide whether to sign the `v0.1.0` tag before creating it. Tag signing is optional
  and does not block committing, pushing, or running CI.

## Release artifacts

`dist-workspace.toml` pins cargo-dist and produces checksummed archives plus a shell installer
for Intel and ARM Linux and macOS. These prebuilt binaries enable the optional `telemetry`
feature. Regenerate `.github/workflows/release.yml` after changing dist configuration:

```sh
dist generate
git diff --exit-code -- .github/workflows/release.yml
```
