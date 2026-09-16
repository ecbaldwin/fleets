# Contributing

Thanks for considering a contribution to fleets.

## Development

Fleets requires Rust 1.85 or newer. Before opening a pull request, run:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features
```

Compatibility changes should include a focused fixture in `tests/fixtures` and pass the
differential suite against ansible-core 2.18.9. Install `ansible-core==2.18.9` and
`tomli-w==1.2.0`, then run:

```sh
FLEETS_REQUIRE_ANSIBLE=1 cargo test --test differential
```

By submitting a contribution, you agree that it is licensed under the project's MIT License.
