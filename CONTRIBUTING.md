# Contributing to Forgejo Actions Orchestrator

## Develop

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Optional git hooks powered by [lefthook](https://github.com/evilmartians/lefthook), which run the same checks plus `shellcheck` before each commit.

```sh
lefthook install
```

### Cross-compile

Builds a reproducible static `x86_64-unknown-linux-musl` binary. Needs zig 0.16.0 and cargo-zigbuild on top of the pinned toolchain.

```sh
./deploy/build.sh    # prints the SHA-256
# -> target/x86_64-unknown-linux-musl/release/forgejo-actions-orchestrator
```
