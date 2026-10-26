#!/usr/bin/env bash
# Tea toolchain gate. Requires the Hydro monorepo as a sibling checkout.
set -euo pipefail

export PATH="$HOME/.cargo/bin:$PATH"
cd "$(dirname "$0")/.."

echo "== fmt =="
cargo fmt --all -- --check

echo "== clippy =="
cargo clippy --all-targets -- -D warnings

echo "== Hydro runtime staticlib =="
cargo build --manifest-path ../hydro/Cargo.toml -p hydro-runtime
mkdir -p target/debug
cp ../hydro/target/debug/libhydro_runtime.a target/debug/libhydro_runtime.a

echo "== unit / E2E / AOT / pytest / interpreter / PGO =="
cargo test

echo "ALL GREEN (Tea)"
