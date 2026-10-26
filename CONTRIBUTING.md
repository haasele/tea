# Contributing to Tea

Tea is the toolchain repository for the sibling Hydro runtime monorepo.

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
./scripts/verify.sh
```

`scripts/verify.sh` builds `../hydro` and copies `libhydro_runtime.a` next to
the test binaries for AOT coverage. Agents edit disjoint files and do not run
builds/tests/formatters themselves.
