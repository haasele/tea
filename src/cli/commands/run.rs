//! `tea run`: compatibility mode that delegates to `uv run`, plus the hidden
//! `__spawn` entry point used by `multiprocessing`.
//!
//! Hydro's own engine entry point is [`super::infuse`]; `run` exists so a
//! project that is driven by uv keeps working unchanged. Every argument is
//! forwarded verbatim in the current working directory and `uv` inherits the
//! terminal, environment and exit status.

use std::env;
use std::ffi::OsString;

use super::infuse::compile_and_run_jit;
use super::uv::run_uv;
use crate::cli::args::{parse_single_source, Verbosity};
use crate::cli::{CliError, CliResult};

pub(crate) fn cmd_run(args: &[OsString], _verbosity: Verbosity) -> CliResult {
    // `uv run` accepts a file, no file, or arguments after `--`; forwarding
    // verbatim keeps all three forms working without tea parsing them.
    run_uv("run", args)
}

/// Hidden entry point used by `multiprocessing` spawn children.
///
/// The parent records the payload path in `HYDRO_SPAWN_PAYLOAD`; `hydro_main`
/// registers the program functions and consumes the payload before any
/// module body would run.
pub(crate) fn cmd_spawn(args: &[OsString], verbosity: Verbosity) -> CliResult {
    if env::var_os("HYDRO_SPAWN_PAYLOAD")
        .filter(|value| !value.is_empty())
        .is_none()
    {
        return Err(CliError::new(
            1,
            "tea: __spawn: HYDRO_SPAWN_PAYLOAD is not set",
        ));
    }
    let (file, options) = parse_single_source(args, "__spawn", verbosity)?;
    compile_and_run_jit(&file, &options, Vec::new())
}
