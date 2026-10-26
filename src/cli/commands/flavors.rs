//! `tea flavors`: list the stored flavor profiles.

use std::ffi::OsString;
use std::process::ExitCode;

use crate::cli::args::{apply_verbosity, Verbosity};
use crate::cli::{CliError, CliResult};
use crate::profile;

pub(crate) fn cmd_flavors(args: &[OsString], verbosity: Verbosity) -> CliResult {
    let mut verbosity = verbosity;
    for arg in args {
        let text = arg.to_string_lossy();
        if let Some(result) = apply_verbosity(text.as_ref(), &mut verbosity, "flavors") {
            result?;
            continue;
        }
        return Err(CliError::usage(format!(
            "tea: flavors: unexpected argument '{text}'"
        )));
    }

    let flavors = profile::list()?;
    if flavors.is_empty() {
        if !verbosity.is_quiet() {
            println!("tea flavors: no flavors stored in {}", profile::FLAVOR_DIR);
        }
        return Ok(ExitCode::SUCCESS);
    }
    for flavor in &flavors {
        println!(
            "{}: {} runs, {} functions",
            flavor.name,
            flavor.runs,
            flavor.function_count()
        );
    }
    Ok(ExitCode::SUCCESS)
}
