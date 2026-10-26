//! `tea infuse`: compile a Python source with the Hydro JIT (or the IR
//! tree-walk interpreter) and run it.
//!
//! This is the original engine entry point. `tea run` is the uv compatibility
//! mode and never reaches this module.

use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use super::{exit_code, read_source, unwrap_or_report};
use crate::cli::args::{parse_run_args, Verbosity};
use crate::cli::diag;
use crate::cli::{CliError, CliResult};

pub(crate) fn cmd_infuse(args: &[OsString], verbosity: Verbosity) -> CliResult {
    let (file, options, program_args, interpret) = parse_run_args(args, "infuse", verbosity)?;
    if interpret {
        return compile_and_run_interpreted(&file, &options, program_args);
    }
    compile_and_run_jit(&file, &options, program_args)
}

/// Compiles and JIT-executes one source file, recording its canonical path so
/// a spawned child can recompile the same program.
pub(crate) fn compile_and_run_jit(
    file: &Path,
    options: &diag::Options,
    program_args: Vec<String>,
) -> CliResult {
    let source = read_source(file)?;
    let frontend_start = Instant::now();
    let program = unwrap_or_report(
        hydro_frontend::check_file(file),
        &source,
        file,
        "compile",
        options,
    )?;
    let frontend = frontend_start.elapsed();
    install_entry(file, program_args);
    let jit_start = Instant::now();
    let code = hydro_codegen::run_jit(&program)
        .map_err(|error| CliError::new(2, format!("tea: internal codegen error: {error}")))?;
    if options.verbosity.is_verbose() {
        eprintln!("tea: infuse: frontend {:.1} ms", millis(frontend));
        eprintln!("tea: infuse: jit {:.1} ms", millis(jit_start.elapsed()));
    }
    Ok(ExitCode::from(exit_code(code)))
}

/// Records the canonical source path and argv before the program starts.
///
/// A relative path would break when a spawned child runs from another
/// directory; canonicalization also matches the source location used by
/// diagnostics.
fn install_entry(file: &Path, program_args: Vec<String>) {
    let canonical = fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf());
    hydro_runtime::set_jit_source(canonical);
    hydro_runtime::set_argv(file.display().to_string(), program_args);
}

fn millis(duration: std::time::Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// Compiles the frontend IR and executes it with the tree-walk interpreter
/// (`tea infuse --interpret`).
///
/// The runtime is initialised exactly once, source/argv are installed like the
/// JIT path does, module initializers run in dependency order and the module
/// body follows. An uncaught exception is printed by the runtime's regular
/// traceback path and becomes the process exit code.
fn compile_and_run_interpreted(
    file: &Path,
    options: &diag::Options,
    program_args: Vec<String>,
) -> CliResult {
    let source = read_source(file)?;
    let frontend_start = Instant::now();
    let program = unwrap_or_report(
        hydro_frontend::check_file(file),
        &source,
        file,
        "compile",
        options,
    )?;
    let frontend = frontend_start.elapsed();

    install_entry(file, program_args);

    hydro_runtime::abi::hydro_runtime_init();
    let source_name = program.source_name.clone();
    hydro_runtime::abi::hydro_set_file(source_name.as_ptr(), source_name.len());

    let mut interp = hydro_interp::Interp::new("__main__");
    let id = interp.add_program(Arc::new(program));

    let interpret_start = Instant::now();
    let completed = interp.run(&id);
    if options.verbosity.is_verbose() {
        eprintln!("tea: infuse: frontend {:.1} ms", millis(frontend));
        eprintln!(
            "tea: infuse: interpret {:.1} ms",
            millis(interpret_start.elapsed())
        );
    }
    if completed {
        return Ok(ExitCode::from(0));
    }
    let code = hydro_runtime::abi::hydro_uncaught() as i32;
    Ok(ExitCode::from(exit_code(code)))
}
