# Tea

Tea is the toolchain for the [Hydro](https://github.com/haasele/hydro) Python
runtime. It owns project scaffolding, the command-line interface, pytest
integration, the interpreter/REPL entry points, AOT packaging, and the
profile-guided **flavor** store. Hydro owns the compiler/runtime monorepo.

> **Status: early development (0.1.0).** The compatibility subset is tracked
> in the Hydro repository. CPython 3.14.9 is the compatibility target.

## Commands

| Command | Behavior |
|---|---|
| `tea run [args...]` | **Compatibility mode**: delegates to `uv run` with the same arguments. |
| `tea infuse <file.py> [args...]` | **Hydro engine**: compile and run with the JIT; `--interpret` uses the IR interpreter. |
| `tea steep [--flavor NAME] [--merge OTHER]... <file.py> [args...]` | Start a profiling session, run through Hydro, and accumulate call counts into `.tea/flavors/NAME.json`. Without `--flavor`, the flavor is `default`. |
| `tea brew <file.py> [-o OUT] [--flavor NAME]` | AOT build. `--flavor` uses a recorded profile for profile-guided code layout. `tea build` remains an alias. |
| `tea flavors` | List stored flavors, run counts and function counts. |
| `tea test` / `tea pytest` | Run test programs / the pytest-compatible runner (engine mode). |
| `tea new/init/check/fmt/lint/clean/gc/doctor/completions` | Project and developer commands. |
| `tea install <wheel.whl> [--target DIR]` | Validate a CPython 3.14 wheel and unpack it into site-packages (`VIRTUAL_ENV`, else `.tea/site-packages`, or `--target`). |
| `tea add/remove/sync/lock` | Unchanged `uv` passthrough; `TEA_UV` overrides the executable. |

## Flavors

A flavor is durable profile data, not a one-shot build setting:

```bash
tea steep --flavor prod app.py          # first run creates prod
tea steep --flavor prod app.py          # later runs add counts
tea steep --flavor prod --merge staging # merge staging into prod
tea brew app.py --flavor prod -o app    # AOT uses the recorded profile
```

The store is `.tea/flavors/<name>.json` with
`{"version":1,"flavor":...,"runs":N,"functions":{name:count}}`. Writes are
atomic; flavor names are restricted to `[A-Za-z0-9._-]+`. Multiple source runs
accumulate; merging sums counts and run counts. Hydro codegen consumes the
profile for hot-function ordering during AOT lowering; an unknown flavor is a
clear CLI error.

## Build

Tea expects the Hydro repository as a sibling checkout:

```bash
git clone https://github.com/haasele/hydro hydro
git clone https://github.com/haasele/tea tea
cd tea
cargo build --release
./target/release/tea --version
```

Build the Hydro runtime staticlib (and copy it beside the Tea test binaries for
AOT tests) with:

```bash
./scripts/verify.sh
```

`tea run` requires `uv` (or `TEA_UV`); `tea infuse` requires only the Hydro
runtime. `tea build` needs a C compiler to link the AOT object.

## Verification

`./scripts/verify.sh` runs fmt, Clippy, Hydro runtime build, the Tea unit/E2E/AOT
suite, pytest, interpreter/REPL tests, and the PGO flavor tests. The Hydro gate
is `../hydro/scripts/verify.sh`.
