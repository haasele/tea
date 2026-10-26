//! `tea help` and `tea --version`.

use std::env;
use std::ffi::OsString;
use std::process::ExitCode;

use crate::cli::args::{apply_verbosity, Verbosity};
use crate::cli::{usage_text, CliError, CliResult};

pub(crate) fn cmd_help(args: &[OsString]) -> CliResult {
    if let Some(extra) = args.first() {
        return Err(CliError::usage(format!(
            "tea: help: unexpected argument '{}'",
            extra.to_string_lossy()
        )));
    }
    print!("{}", usage_text());
    Ok(ExitCode::SUCCESS)
}

pub(crate) fn cmd_version(args: &[OsString], verbosity: Verbosity) -> CliResult {
    let mut verbosity = verbosity;
    for extra in args {
        let text = extra.to_string_lossy();
        if let Some(result) = apply_verbosity(text.as_ref(), &mut verbosity, "version") {
            result?;
            continue;
        }
        return Err(CliError::usage(format!(
            "tea: version: unexpected argument '{text}'"
        )));
    }

    let version = env!("CARGO_PKG_VERSION");
    println!("tea {version}");
    if !verbosity.is_verbose() {
        return Ok(ExitCode::SUCCESS);
    }

    // The runtime is built from this workspace, so its package version is the
    // workspace version; there is no separate `hydro_runtime::VERSION` constant.
    println!("target: {}-{}", env::consts::ARCH, env::consts::OS);
    println!("backend: Cranelift");
    println!("runtime: workspace {version}");
    println!(
        "python: {} compatibility target (ABI {})",
        hydro_ir::PYTHON_VERSION_STRING,
        hydro_ir::PYTHON_ABI_TAG
    );
    Ok(ExitCode::SUCCESS)
}
