//! Command-line handling for the `tea` binary.
//!
//! Layout:
//! - [`args`] parses the flags shared by the subcommands,
//! - [`dispatch`] maps the first positional argument to a command,
//! - [`commands`] holds one module per subcommand,
//! - [`diag`] renders frontend diagnostics.

pub(crate) mod args;
pub(crate) mod commands;
pub(crate) mod diag;
pub(crate) mod dispatch;

use std::process::ExitCode;

/// A failure that terminates the CLI.
///
/// `message` is `None` when the command already printed its output (for
/// example a list of frontend diagnostics or the C compiler's stderr).
#[derive(Debug)]
pub(crate) struct CliError {
    pub(crate) code: u8,
    pub(crate) message: Option<String>,
}

impl CliError {
    pub(crate) fn new(code: u8, message: impl Into<String>) -> CliError {
        CliError {
            code,
            message: Some(message.into()),
        }
    }

    pub(crate) fn silent(code: u8) -> CliError {
        CliError {
            code,
            message: None,
        }
    }

    pub(crate) fn usage(message: impl Into<String>) -> CliError {
        CliError::new(2, format!("{}\n\n{}", message.into(), usage_text()))
    }
}

pub(crate) type CliResult = Result<ExitCode, CliError>;

pub(crate) fn usage_text() -> String {
    format!(
        "tea {}\n\n\
         usage:\n  \
         tea new <path> [--force]           create a project directory with a minimal scaffold\n  \
         tea init [path] [--force]          scaffold a project in an existing directory\n  \
         tea check <file.py> [options]      parse and type-check only\n  \
         tea infuse <file.py> [options]     compile with the Hydro JIT and run\n  \
         tea infuse --interpret <file.py>   run the IR tree-walk interpreter\n  \
         tea run [args...]                  run through uv (compatibility mode)\n  \
         tea steep [options] <file.py>      profile a JIT run into a flavor\n  \
         tea flavors                        list stored flavors\n  \
         tea repl [--no-echo] [--no-banner]  run a persistent interpreter session\n  \
         tea -c <code> [args...]            run code with the interpreter (sys.argv[0] = -c)\n  \
         tea -i [<file> | -c <code>] [args...]  run the file/code, then enter the interpreter\n  \
         tea gc [--json] <file.py> [options]   run and report GC statistics\n  \
         tea build <file.py> [options]      compile and link a native executable\n  \
         tea brew <file.py> [options]       AOT build, optionally with --flavor\n  \
         tea test [paths...] [options]      run test_*.py / *_test.py files with `tea infuse`\n  \
         tea pytest [paths...] [options]    run pytest-style tests, one process per file\n  \
         tea fmt [paths...] [--check]       strip trailing whitespace and fix the final newline\n  \
         tea lint [paths...] [options]      type-check every .py file in the given paths\n  \
         tea clean [--dry-run]              remove .tea/cache and target/tea-tmp\n  \
         tea install <wheel.whl> [--target DIR]  unpack a compatible wheel into site-packages\n  \
         tea add <spec...> [uv options]     add dependencies with uv\n  \
         tea remove <spec...> [uv options]  remove dependencies with uv\n  \
         tea sync [uv options]              sync the environment with uv\n  \
         tea lock [uv options]              update the lockfile with uv\n  \
         tea doctor [--json]                report toolchain and environment status\n  \
         tea completions <shell>            print a completion script (bash, zsh or fish)\n  \
         tea help | --help | -h             show this help\n  \
         tea --version | -V                 show the version (--verbose adds details)\n\n\
         global options (before the command or its first path):\n  \
         --verbose                print build/run phase timings and build cache decisions\n  \
         --quiet                  suppress non-error status output\n  \
         --verbose and --quiet cannot be combined\n\n\
         new/init:\n  \
         a scaffold contains main.py, pyproject.toml and .gitignore; without --force\n  \
         existing non-empty files are never overwritten\n\n\
         test/fmt/lint:\n  \
         paths default to the current directory and are searched recursively\n  \
         (hidden entries, target, __pycache__ and node_modules are skipped);\n  \
         explicit files are always used as given\n  \
         a test passes when its program exits with status 0; there are no\n  \
         pytest-style assertions\n  \
         pytest runs test_*.py / *_test.py files in a fresh child process each,\n  \
         with `pytest.fixture`, `pytest.mark.parametrize`, `raises`, `approx`,\n  \
         skip/xfail markers and ancestor conftest.py files\n  \
         -q, --quiet              (pytest) print one status character per test\n  \
         -x, --exitfirst          (pytest) stop after the first failure\n  \
         --maxfail=N              (pytest) stop after N failures\n  \
         -k EXPR                  (pytest) only run tests matching EXPR\n  \
                                  (substring, `not`, `and`, `or`)\n  \
         --json                   (pytest) print the aggregated result as JSON\n  \
                                  exit codes: 0 pass, 1 failure, 2 collection,\n  \
                                  5 no tests\n  \
         --check                  (fmt) report files that need formatting, do not write\n  \
         --json                   (test) print a JSON array of results on stdout\n  \
                                  (lint) print diagnostics as a JSON array on stderr\n  \
         --color=<WHEN>           colorize output: auto, always or never (default: auto)\n  \
         --no-color               disable colors (same as --color=never)\n\n\
         repl / -c / -i:\n  \
         one persistent tree-walk interpreter session: every input is compiled\n  \
         and executed in the same namespace, so earlier bindings, functions and\n  \
         classes stay usable and imports are initialized once\n  \
         the value of a trailing top-level expression is printed and stored in\n  \
         `_`; assignments and compound statements print nothing\n  \
         prompts and the banner go to stderr and only appear when stdin is a\n  \
         terminal; program output and echoed values go to stdout\n  \
         --no-echo                do not echo expression values\n  \
         --no-banner              do not print the version banner\n  \
         --quiet                  suppress the banner and prompts\n\n\
         diagnostic options (check, infuse, gc, build, brew, steep, test, lint, repl, -c, -i):\n  \
         --json                   print diagnostics as a JSON array on stderr\n  \
         --color=<WHEN>           colorize diagnostics: auto, always or never (default: auto)\n  \
         --no-color               disable colors (same as --color=never)\n\n\
         gc:\n  \
         the program is compiled and run exactly like `tea infuse`; the GC\n  \
         statistics are printed to stdout afterwards (after program output)\n  \
         --json                   print the statistics as one JSON object on stdout\n  \
                                  (also selects JSON diagnostics on stderr)\n\n\
         build options:\n  \
         -o, --output <OUT>       output path (default: <file stem> in the current directory)\n  \
         --runtime-lib <PATH>     path to libhydro_runtime.a\n  \
         --flavor <NAME>          use hot functions from .tea/flavors/<NAME>.json\n  \
         --no-cache               do not read or write the build cache (.tea/cache)\n\n\
         steep / flavors:\n  \
         steep runs the Hydro JIT with profile collection enabled, dumps the\n  \
         runtime counters and merges them into .tea/flavors/<NAME>.json\n  \
         (default flavor: default); repeated runs accumulate\n  \
         --flavor <NAME>          flavor to record (default: default)\n  \
         --merge <OTHER>          merge an existing flavor into NAME (repeatable;\n  \
                                  without a source file only the merge is performed)\n  \
         --show                   print the ten hottest functions after the summary\n  \
         flavors lists each stored flavor with its runs and function count\n  \
         a flavor file is JSON: {{\"version\":1,\"flavor\":\"name\",\"runs\":N,\n  \
         \"functions\":{{\"name\":count}}}}\n\n\
         uv commands (run, add, remove, sync, lock):\n  \
         tea run forwards its arguments verbatim to `uv run` in the current\n  \
         directory; add/remove/sync/lock forward their arguments verbatim too\n  \
         tea adds no options of its own and returns the exit status of uv\n\n\
         install:\n  \
         the wheel must target CPython 3.14 (py3-none-any, py3-none-<platform>,\n  \
         cp314-* or an abi3 wheel) and a compatible platform; members are validated\n  \
         before anything is written and unpacked through a temporary directory\n  \
         --target DIR             install into DIR (default: VIRTUAL_ENV/lib/python3.14/\n  \
                                  site-packages, else .tea/site-packages)\n  \
         HYDRO_SITE_PACKAGES=<DIR> makes an explicit target visible to tea infuse\n\n\
         environment:\n  \
         CC                       C compiler used to link AOT binaries (default: cc)\n  \
         HYDRO_RUNTIME_LIB          path to libhydro_runtime.a\n  \
         HYDRO_SITE_PACKAGES      site-packages directories searched for modules and\n  \
                                  extensions (before VIRTUAL_ENV/CONDA_PREFIX)\n  \
         TEA_UV                   uv executable used by run/add/remove/sync/lock (default: uv)\n  \
         NO_COLOR                 disable colored diagnostics when set\n\n\
         exit codes: 0 success, 1 user/program error, 2 internal/setup error\n",
        env!("CARGO_PKG_VERSION")
    )
}
