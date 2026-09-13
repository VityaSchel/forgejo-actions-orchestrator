# Contributing to Forgejo Actions Orchestrator

## Develop

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Optional [lefthook](https://github.com/evilmartians/lefthook) hooks run rustfmt, clippy, tests and `shellcheck` before each commit. rustfmt fixes staged files instead of failing the commit.

```sh
lefthook install
```

### Run locally

```sh
FORGEJO_RUNNER_TOKEN=… FORGEJO_STATUS_TOKEN=… HETZNER_TOKEN=… cargo run -- --config config.toml
```

### Provider prices

`scripts/pricing.py` regenerates the price list at the end of `config.example.toml`. It needs `HETZNER_TOKEN`, `GCORE_TOKEN` and `GCORE_PROJECT_ID`. Vultr, Cherry Servers and Scaleway prices need no credentials.

```sh
python3 scripts/pricing.py           # print
python3 scripts/pricing.py --write   # rewrite config.example.toml
```

### Cross-compile

`deploy/build.sh` builds a reproducible static `x86_64-unknown-linux-musl` binary. It needs cargo-zigbuild and zig 0.16.0 on top of the pinned toolchain.

Install zig from the [official tarball](https://ziglang.org/download/). A package manager's zig linked against system LLVM reports the same `zig version` but emits different machine code, so `build.sh` rejects any zig whose clang differs from the tarball's.

```sh
./deploy/build.sh    # prints the SHA-256
# -> target/x86_64-unknown-linux-musl/release/forgejo-actions-orchestrator
```

Canonical releases builder in release.yml CI workflow is Linux x86_64.
