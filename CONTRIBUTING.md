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

Builds a reproducible static `x86_64-unknown-linux-musl` binary. Needs cargo-zigbuild and zig 0.16.0 on top of the pinned toolchain.

Install zig from the [official tarball](https://ziglang.org/download/), not from a package manager. Don't link zig against a rolling system LLVM, otherwise two installs reporting the same `zig version` carry different clang and LLD builds and emit different machine code. `build.sh` refuses any zig whose clang is not the one the tarball ships.

```sh
./deploy/build.sh    # prints the SHA-256
# -> target/x86_64-unknown-linux-musl/release/forgejo-actions-orchestrator
```

The hash reproduces on a given host OS, not across them. Released binaries therefore come from the `Release` workflow, run manually in [Actions](https://git.hloth.dev/hloth/forgejo-actions-orchestrator/releases).
