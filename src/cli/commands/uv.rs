//! `tea run`, `add`, `remove`, `sync` and `lock`: a thin passthrough to `uv`.
//!
//! Every argument is forwarded verbatim in the current working directory and
//! `uv` inherits the terminal and environment. Tea does not interpret source
//! files, package specs, options or `pyproject.toml`; `uv` owns those decisions
//! and its exit status is returned. `TEA_UV` selects a different executable
//! (tests, alternative installations).

use std::env;
use std::ffi::OsString;
use std::process::{Command, ExitCode};

use crate::cli::{CliError, CliResult};

pub(crate) fn cmd_add(args: &[OsString]) -> CliResult {
    run_uv("add", args)
}

pub(crate) fn cmd_remove(args: &[OsString]) -> CliResult {
    run_uv("remove", args)
}

pub(crate) fn cmd_sync(args: &[OsString]) -> CliResult {
    run_uv("sync", args)
}

pub(crate) fn cmd_lock(args: &[OsString]) -> CliResult {
    run_uv("lock", args)
}

/// The `uv` executable, overridable through `TEA_UV`.
fn uv_program() -> OsString {
    env::var_os("TEA_UV")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| OsString::from("uv"))
}

/// Runs `uv <subcommand> <args...>` and maps its status to a `tea` exit code.
///
/// A missing executable is a setup error (2); a signal-terminated child is
/// reported as a user error (1) because `uv` never produced an exit status.
pub(crate) fn run_uv(subcommand: &str, args: &[OsString]) -> CliResult {
    let program = uv_program();
    let status = Command::new(&program)
        .arg(subcommand)
        .args(args)
        .status()
        .map_err(|error| {
            CliError::new(
                2,
                format!(
                    "tea: {subcommand}: cannot run '{}': {error}\n\
                 tea: install uv or set TEA_UV to the uv executable",
                    program.to_string_lossy()
                ),
            )
        })?;
    match status.code() {
        Some(code) => Ok(ExitCode::from(u8::try_from(code).unwrap_or(1))),
        None => Err(CliError::new(
            1,
            format!("tea: {subcommand}: uv terminated by a signal"),
        )),
    }
}
