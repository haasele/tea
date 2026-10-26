# AGENTS.md — Tea Work Status

> **Resume rule:** read this file and the sibling Hydro `AGENTS.md` before
> continuing. Tea is the toolchain repository for the Hydro runtime; Hydro is
> a separate monorepo (`../hydro`).

## Project

Tea is the CLI/toolchain for the Hydro Python runtime:
`tea run` (uv compatibility), `tea infuse` (Hydro JIT), `tea steep` (PGO
profile flavors), `tea brew`/`tea build` (AOT with optional profile), plus
project, pytest, formatter, linter, REPL and diagnostics commands.

Compatibility target and language scope are owned by Hydro (CPython 3.14.9).
Tea keeps the CLI/E2E/AOT/pytest/interpreter tests.

## Current State

| Area | Status |
|---|---|
| CLI | `run` compatibility mode, `infuse`, `check`, `build`/`brew`, `test`, `pytest`, `repl`, project/fmt/lint/clean, uv passthrough |
| PGO flavors | `.tea/flavors/<name>.json`, steep accumulation/merge, AOT hot-function profile use |
| AOT | object + `cc` link with `libhydro_runtime.a`; no `libpython` |
| E2E/AOT tests | run through `CARGO_BIN_EXE_tea` (325 tests total) |

## Commands

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo build --release
cargo test
./scripts/verify.sh          # builds ../hydro runtime staticlib, then tests
```

The gate requires a sibling Hydro checkout. For AOT tests, the Hydro runtime
staticlib is copied into `target/debug/libhydro_runtime.a`.

## Working agreement

- Every agent owns disjoint files; do not edit Hydro source from this repo.
- Agents edit only; the orchestrator builds/tests/formats.
- No CLI feature should fake Hydro behavior; `run` must delegate to `uv`,
  `infuse` must use Hydro, and flavors must feed real codegen data.
