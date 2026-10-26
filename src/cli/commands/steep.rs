//! `tea steep`: profile a JIT run and accumulate the counts in a flavor.
//!
//! A steep run starts a fresh profiling session (`hydro_profile_reset`), runs
//! the program through the JIT with profile collection enabled, dumps the
//! runtime counters to a temporary file and merges them into
//! `.tea/flavors/<name>.json`. Repeated runs accumulate; `--merge` folds
//! existing flavors into the target without running a program.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use super::{exit_code, read_source, unwrap_or_report};
use crate::cli::args::{parse_steep_args, Verbosity};
use crate::cli::{CliError, CliResult};
use crate::profile::{self, Flavor};

pub(crate) fn cmd_steep(args: &[OsString], verbosity: Verbosity) -> CliResult {
    let options = parse_steep_args(args, verbosity)?;
    let quiet = options.diagnostics.verbosity.is_quiet();
    let mut flavor = profile::load_optional(&options.flavor)?;

    if !options.merges.is_empty() {
        for source in &options.merges {
            let other = profile::load(source)?;
            flavor.merge(&other);
        }
        // A merge is durable on its own; a later run failure must not lose it.
        flavor.save()?;
        // A merge without a program is a pure bookkeeping operation.
        if options.file.is_none() {
            if !quiet {
                print_summary(&flavor);
                if options.show {
                    print_top(&flavor);
                }
            }
            return Ok(ExitCode::SUCCESS);
        }
    }

    let Some(file) = options.file.as_ref() else {
        return Err(CliError::usage("tea: steep: missing source file"));
    };
    let source = read_source(file)?;
    let frontend_start = Instant::now();
    let program = unwrap_or_report(
        hydro_frontend::check_file(file),
        &source,
        file,
        "compile",
        &options.diagnostics,
    )?;
    let frontend = frontend_start.elapsed();

    let canonical = fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf());
    hydro_runtime::set_jit_source(canonical);
    hydro_runtime::set_argv(file.display().to_string(), options.program_args.clone());

    // A fresh session: counters from an earlier in-process run must not leak
    // into this flavor.
    hydro_runtime::abi::hydro_profile_reset();

    let codegen_options = hydro_codegen::CodegenOptions {
        collect_profile: true,
        ..hydro_codegen::CodegenOptions::default()
    };
    let jit_start = Instant::now();
    let code = hydro_codegen::run_jit_with_options(&program, &codegen_options)
        .map_err(|error| CliError::new(2, format!("tea: internal codegen error: {error}")))?;
    if !quiet && options.diagnostics.verbosity.is_verbose() {
        eprintln!("tea: steep: frontend {:.1} ms", millis(frontend));
        eprintln!("tea: steep: jit {:.1} ms", millis(jit_start.elapsed()));
    }

    let dump = dump_profile()?;
    let counts = profile::parse_dump(&dump)?;
    flavor.record_run(&counts);
    flavor.save()?;

    if !quiet {
        print_summary(&flavor);
        if options.show {
            print_top(&flavor);
        }
    }
    Ok(ExitCode::from(exit_code(code)))
}

fn millis(duration: std::time::Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// Dumps the process-global profile to a temporary file and returns its text.
///
/// The runtime writes and closes the file synchronously, so it can be read
/// immediately and removed afterwards.
fn dump_profile() -> Result<String, CliError> {
    let path = temporary_dump_path();
    let encoded = path.to_string_lossy().into_owned().into_bytes();
    let status = hydro_runtime::abi::hydro_profile_dump(encoded.as_ptr(), encoded.len());
    if status != 0 {
        let _ = fs::remove_file(&path);
        return Err(CliError::new(
            2,
            format!(
                "tea: steep: cannot write the profile dump to '{}'",
                path.display()
            ),
        ));
    }
    let result = fs::read_to_string(&path);
    let _ = fs::remove_file(&path);
    result.map_err(|error| {
        CliError::new(
            2,
            format!(
                "tea: steep: cannot read the profile dump '{}': {error}",
                path.display()
            ),
        )
    })
}

fn temporary_dump_path() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    env::temp_dir().join(format!("tea-profile-{}-{nanos}.txt", std::process::id()))
}

fn print_summary(flavor: &Flavor) {
    eprintln!(
        "tea steep: flavor '{}': {} functions, {} runs",
        flavor.name,
        flavor.function_count(),
        flavor.runs
    );
}

/// Prints the ten hottest functions of the flavor, highest count first.
fn print_top(flavor: &Flavor) {
    let mut entries: Vec<(&String, &u64)> = flavor.functions.iter().collect();
    entries.sort_by(|left, right| right.1.cmp(left.1).then(left.0.cmp(right.0)));
    for (name, count) in entries.into_iter().take(10) {
        eprintln!("tea steep:   {count:>8}  {name}");
    }
}
