//! `tea build` / `tea brew`: AOT-compile a program, link it and mark it
//! executable. `brew` is the flavor-oriented name and also accepts `--flavor`.

use std::env;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use super::{read_source, unwrap_or_report};
use crate::cli::args::{parse_build, Verbosity};
use crate::cli::{CliError, CliResult};
use crate::profile;

const RUNTIME_LIB_NAME: &str = "libhydro_runtime.a";

/// Build cache location, relative to the working directory.
const CACHE_DIR: &str = ".tea/cache";

pub(crate) fn cmd_build(args: &[OsString], verbosity: Verbosity) -> CliResult {
    build(args, verbosity, "build")
}

/// `tea brew`: the AOT build under its flavor-oriented name.
pub(crate) fn cmd_brew(args: &[OsString], verbosity: Verbosity) -> CliResult {
    build(args, verbosity, "brew")
}

fn build(args: &[OsString], verbosity: Verbosity, command: &str) -> CliResult {
    let options = parse_build(args, command, verbosity)?;
    let verbose = options.diagnostics.verbosity.is_verbose();
    // Loading the flavor before doing any work makes a typo fail fast.
    let flavor = match options.flavor.as_deref() {
        Some(name) => Some(profile::load(name)?),
        None => None,
    };
    let source = read_source(&options.file)?;
    let frontend_start = Instant::now();
    let program = unwrap_or_report(
        hydro_frontend::check_file(&options.file),
        &source,
        &options.file,
        "compile",
        &options.diagnostics,
    )?;
    if verbose {
        eprintln!(
            "tea: {command}: frontend {:.1} ms",
            millis(frontend_start.elapsed())
        );
        if let Some(flavor) = &flavor {
            eprintln!(
                "tea: {command}: flavor '{}' ({} hot functions)",
                flavor.name,
                flavor.function_count()
            );
        }
    }

    let output = options
        .output
        .unwrap_or_else(|| default_output(&options.file));
    let runtime = resolve_runtime_lib(options.runtime_lib.as_deref())?;

    // The cache is an optimization only: every failure below is non-fatal and
    // falls back to a plain build.
    let cache = if options.no_cache {
        None
    } else {
        BuildCache::open()
    };
    let key = cache.as_ref().map(|_| {
        cache_key(
            &source,
            &options.file,
            &runtime,
            &flavor_cache_key(flavor.as_ref()),
        )
    });
    if verbose {
        match (&cache, &key, options.no_cache) {
            (_, _, true) => eprintln!("tea: {command}: cache disabled"),
            (Some(_), Some(key), _) => eprintln!("tea: {command}: cache key {key}"),
            _ => eprintln!("tea: {command}: cache unavailable"),
        }
    }
    if let (Some(cache), Some(key)) = (&cache, &key) {
        if cache.restore(key, &output) {
            if verbose {
                eprintln!("tea: {command}: cache hit");
            }
            return Ok(ExitCode::SUCCESS);
        }
        if verbose {
            eprintln!("tea: {command}: cache miss");
        }
    }

    let codegen_start = Instant::now();
    let object = compile_object(&program, flavor.as_ref())
        .map_err(|error| CliError::new(2, format!("tea: internal codegen error: {error}")))?;
    if verbose {
        eprintln!(
            "tea: {command}: codegen {:.1} ms",
            millis(codegen_start.elapsed())
        );
    }

    // The guard removes the temporary object on every exit path, including
    // compiler/linker failures and write errors.
    let temporary = create_temp_object(&object)?;

    if let (Some(cache), Some(key)) = (&cache, &key) {
        cache.store_object(key, &object);
        let cached_binary = cache.binary(key);
        let staging = cache.staging(key);
        // Linking into the cache is best effort: if it fails (for example a
        // read-only cache) the plain build below still produces the output and
        // reports the linker diagnostics. A genuine link error therefore only
        // fails the command through the direct attempt.
        let link_start = Instant::now();
        if run_linker(&temporary.path, &runtime, &staging).is_ok() {
            if verbose {
                eprintln!(
                    "tea: {command}: link {:.1} ms (cache)",
                    millis(link_start.elapsed())
                );
            }
            if fs::rename(&staging, &cached_binary).is_ok() {
                if fs::copy(&cached_binary, &output).is_ok() {
                    make_executable(&output)?;
                    return Ok(ExitCode::SUCCESS);
                }
            } else {
                let _ = fs::remove_file(&staging);
            }
        } else {
            let _ = fs::remove_file(&staging);
        }
        // Cache publication failed; fall through to the direct link.
    }

    let link_start = Instant::now();
    run_linker(&temporary.path, &runtime, &output)
        .map_err(|failure| report_link_failure(failure, &output))?;
    if verbose {
        eprintln!(
            "tea: {command}: link {:.1} ms",
            millis(link_start.elapsed())
        );
    }
    make_executable(&output)?;
    Ok(ExitCode::SUCCESS)
}

fn millis(duration: std::time::Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// Compiles the program, passing the flavor's hot counts when one is selected.
fn compile_object(
    program: &hydro_ir::Program,
    flavor: Option<&profile::Flavor>,
) -> Result<Vec<u8>, hydro_codegen::CodegenError> {
    match flavor {
        None => hydro_codegen::compile_object(program),
        Some(flavor) => {
            let options = hydro_codegen::CodegenOptions {
                collect_profile: false,
                hot_functions: flavor.hot_functions(),
            };
            hydro_codegen::compile_object_with_options(program, &options)
        }
    }
}

/// Stable representation of a flavor for the build cache key.
fn flavor_cache_key(flavor: Option<&profile::Flavor>) -> String {
    match flavor {
        None => String::new(),
        Some(flavor) => {
            let mut key = flavor.name.clone();
            for (function, count) in &flavor.functions {
                key.push('\n');
                key.push_str(function);
                key.push('\t');
                key.push_str(&count.to_string());
            }
            key
        }
    }
}

/// Read/write access to `.tea/cache`.
struct BuildCache {
    directory: PathBuf,
    process_id: u32,
}

impl BuildCache {
    /// Opens the cache, creating it if needed; `None` when unavailable.
    fn open() -> Option<BuildCache> {
        let directory = PathBuf::from(CACHE_DIR);
        fs::create_dir_all(&directory).ok()?;
        Some(BuildCache {
            directory,
            process_id: std::process::id(),
        })
    }

    fn object(&self, key: &str) -> PathBuf {
        self.directory.join(format!("{key}.o"))
    }

    fn binary(&self, key: &str) -> PathBuf {
        self.directory.join(format!("{key}.bin"))
    }

    fn staging(&self, key: &str) -> PathBuf {
        self.directory
            .join(format!("{key}.bin.{}.tmp", self.process_id))
    }

    /// Copies a cached binary to `output`; returns `false` on a cache miss or
    /// any cache-side failure.
    fn restore(&self, key: &str, output: &Path) -> bool {
        let binary = self.binary(key);
        if !binary.is_file() {
            return false;
        }
        if fs::copy(&binary, output).is_err() {
            return false;
        }
        make_executable(output).is_ok()
    }

    /// Writes the object file; failures are deliberately ignored.
    fn store_object(&self, key: &str, bytes: &[u8]) {
        let target = self.object(key);
        let temporary = self
            .directory
            .join(format!("{key}.o.{}.tmp", self.process_id));
        if fs::write(&temporary, bytes).is_ok() && fs::rename(&temporary, &target).is_ok() {
            return;
        }
        let _ = fs::remove_file(&temporary);
    }
}

/// Directory names that never contain an importable module for the entry.
const IGNORED_MODULE_DIRS: [&str; 3] = ["target", ".tea", "__pycache__"];

/// Derives a deterministic cache key from everything that affects the linked
/// binary: source text, the Python modules reachable from the entry, build
/// profile, toolchain version and the identity of the runtime archive.
/// `DefaultHasher` is process-stable and needs no extra dependency; the cache
/// is a local optimization, not a shared artifact store.
fn cache_key(source: &str, entry: &Path, runtime: &Path, flavor: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    b"tea-build-cache-v3".hash(&mut hasher);
    env!("CARGO_PKG_VERSION").hash(&mut hasher);
    // Cache entries contain code generated by this exact executable. A
    // frontend/codegen change without a version bump must not reuse them, so
    // fold the compiler binary's size and mtime into the key.
    if let Ok(executable) = std::env::current_exe() {
        if let Ok(metadata) = fs::metadata(executable) {
            metadata.len().hash(&mut hasher);
            if let Ok(modified) = metadata.modified() {
                if let Ok(duration) = modified.duration_since(UNIX_EPOCH) {
                    duration.as_nanos().hash(&mut hasher);
                }
            }
        }
    }
    env::consts::ARCH.hash(&mut hasher);
    env::consts::OS.hash(&mut hasher);
    // Cache entries are per toolchain profile; a future debug/release split
    // must never collide with the default AOT profile.
    "aot".hash(&mut hasher);
    // Hot-function hints change the emitted object even for identical source.
    flavor.hash(&mut hasher);
    source.hash(&mut hasher);
    // `check_file` resolves imports relative to the entry, so every module in
    // the entry's directory tree can change the compiled program.
    for (relative, contents) in module_files(entry_root(entry)) {
        relative.hash(&mut hasher);
        contents.hash(&mut hasher);
    }
    if let Ok(metadata) = fs::metadata(runtime) {
        metadata.len().hash(&mut hasher);
        if let Ok(modified) = metadata.modified() {
            if let Ok(duration) = modified.duration_since(UNIX_EPOCH) {
                duration.as_nanos().hash(&mut hasher);
            }
        }
    }
    format!("{:016x}", hasher.finish())
}

/// Directory the entry's imports are resolved against.
fn entry_root(entry: &Path) -> &Path {
    match entry.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// All `.py` files below `root`, sorted by relative path, ignoring hidden
/// entries and build/cache directories. A file that cannot be read is kept as
/// `None`: its path still separates cache entries while the build proceeds.
fn module_files(root: &Path) -> Vec<(String, Option<Vec<u8>>)> {
    fn walk(root: &Path, directory: &Path, files: &mut Vec<(String, Option<Vec<u8>>)>) {
        let Ok(entries) = fs::read_dir(directory) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || IGNORED_MODULE_DIRS.contains(&name.as_ref()) {
                continue;
            }
            // Symlinks are skipped so a cycle cannot make the scan hang.
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if file_type.is_dir() {
                walk(root, &path, files);
            } else if file_type.is_file() && path.extension().is_some_and(|ext| ext == "py") {
                let relative = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .into_owned();
                files.push((relative, fs::read(&path).ok()));
            }
        }
    }

    let mut files = Vec::new();
    walk(root, root, &mut files);
    files.sort_by(|left, right| left.0.cmp(&right.0));
    files
}

/// Default output path: the input file stem, relative to the current directory.
fn default_output(file: &Path) -> PathBuf {
    match file.file_stem() {
        Some(stem) if !stem.is_empty() => PathBuf::from(stem),
        _ => PathBuf::from("a.out"),
    }
}

fn resolve_runtime_lib(explicit: Option<&Path>) -> Result<PathBuf, CliError> {
    if let Some(path) = explicit {
        return if path.is_file() {
            Ok(path.to_path_buf())
        } else {
            Err(runtime_lib_error(Some(path)))
        };
    }

    if let Some(value) = env::var_os("HYDRO_RUNTIME_LIB").filter(|value| !value.is_empty()) {
        let path = PathBuf::from(value);
        return if path.is_file() {
            Ok(path)
        } else {
            Err(runtime_lib_error(Some(&path)))
        };
    }

    let candidates = runtime_lib_candidates();
    for candidate in &candidates {
        if candidate.is_file() {
            return Ok(candidate.clone());
        }
    }
    Err(runtime_lib_error(None))
}

/// Default locations searched for `libhydro_runtime.a`, in priority order.
fn runtime_lib_candidates() -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(directory) = env::current_exe()
        .ok()
        .and_then(|executable| executable.parent().map(Path::to_path_buf))
    {
        candidates.push(directory.join(RUNTIME_LIB_NAME));
    }
    if let Ok(directory) = env::current_dir() {
        candidates.push(
            directory
                .join("target")
                .join("debug")
                .join(RUNTIME_LIB_NAME),
        );
        candidates.push(
            directory
                .join("target")
                .join("release")
                .join(RUNTIME_LIB_NAME),
        );
    }
    candidates
}

/// Runtime library path reported by `doctor` plus whether it exists.
///
/// Unlike [`resolve_runtime_lib`] this never fails: a report must work even on
/// a machine where no staticlib was built yet.
pub(crate) fn runtime_lib_status() -> (PathBuf, bool) {
    if let Some(value) = env::var_os("HYDRO_RUNTIME_LIB").filter(|value| !value.is_empty()) {
        let path = PathBuf::from(value);
        let found = path.is_file();
        return (path, found);
    }

    let candidates = runtime_lib_candidates();
    for candidate in &candidates {
        if candidate.is_file() {
            return (candidate.clone(), true);
        }
    }
    (
        candidates
            .into_iter()
            .next()
            .unwrap_or_else(|| PathBuf::from(RUNTIME_LIB_NAME)),
        false,
    )
}

fn runtime_lib_error(path: Option<&Path>) -> CliError {
    let message = match path {
        Some(path) => format!(
            "tea: runtime library not found at '{}'; build it with \
             `cargo build -p tea-runtime` or pass --runtime-lib <PATH>",
            path.display()
        ),
        None => format!(
            "tea: cannot find '{RUNTIME_LIB_NAME}'; build it with \
             `cargo build -p tea-runtime` or pass --runtime-lib <PATH>"
        ),
    };
    CliError::new(2, message)
}

/// Temporary object file that is removed when dropped.
struct TempObject {
    path: PathBuf,
}

impl Drop for TempObject {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn create_temp_object(bytes: &[u8]) -> Result<TempObject, CliError> {
    let directory = env::temp_dir();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let mut last_collision = None;

    for attempt in 0..32u32 {
        let path = directory.join(format!(
            "tea-{}-{}-{}.o",
            std::process::id(),
            nanos,
            attempt
        ));
        // `create_new` guarantees we never clobber another process's object.
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                let temporary = TempObject { path };
                file.write_all(bytes).map_err(|error| {
                    CliError::new(
                        2,
                        format!(
                            "tea: cannot write temporary object '{}': {error}",
                            temporary.path.display()
                        ),
                    )
                })?;
                return Ok(temporary);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                last_collision = Some(error);
            }
            Err(error) => {
                return Err(CliError::new(
                    2,
                    format!(
                        "tea: cannot create temporary object '{}': {error}",
                        path.display()
                    ),
                ))
            }
        }
    }

    let detail = last_collision
        .map(|error| format!(": {error}"))
        .unwrap_or_default();
    Err(CliError::new(
        2,
        format!(
            "tea: cannot create a unique temporary object in '{}'{detail}",
            directory.display()
        ),
    ))
}

/// A linker invocation that did not produce a binary.
struct LinkFailure {
    compiler: OsString,
    detail: LinkFailureDetail,
}

enum LinkFailureDetail {
    /// The compiler binary could not be started.
    Spawn(io::Error),
    /// The compiler ran and failed; `stderr` is already decoded.
    Status { stderr: String, status: String },
}

/// Runs the C compiler. Diagnostics are returned instead of printed so the
/// cache path can retry a direct link without duplicating compiler output.
fn run_linker(object: &Path, runtime: &Path, output: &Path) -> Result<(), LinkFailure> {
    let compiler = match env::var_os("CC").filter(|value| !value.is_empty()) {
        Some(value) => value,
        None => OsString::from("cc"),
    };

    let mut command = Command::new(&compiler);
    command.arg(object).arg(runtime).arg("-o").arg(output);
    // Extensions loaded at run time resolve the CPython C API against the
    // process image, so the executable must export its dynamic symbols. The
    // abi3 extension itself needs no `libpython`: the interpreter symbols it
    // uses are already present in this process.
    if cfg!(target_os = "linux") {
        command.arg("-Wl,--export-dynamic");
    }
    command.arg("-lpthread").arg("-ldl").arg("-lm");

    let result = command.output().map_err(|error| LinkFailure {
        compiler: compiler.clone(),
        detail: LinkFailureDetail::Spawn(error),
    })?;

    if result.status.success() {
        return Ok(());
    }

    let status = match result.status.code() {
        Some(code) => format!("exit code {code}"),
        None => "terminated by a signal".to_string(),
    };
    Err(LinkFailure {
        compiler,
        detail: LinkFailureDetail::Status {
            stderr: String::from_utf8_lossy(&result.stderr).into_owned(),
            status,
        },
    })
}

/// Prints a linker failure and returns the toolchain exit code.
fn report_link_failure(failure: LinkFailure, output: &Path) -> CliError {
    let compiler = failure.compiler.to_string_lossy().into_owned();
    match failure.detail {
        LinkFailureDetail::Spawn(error) => CliError::new(
            2,
            format!("tea: failed to run C compiler '{compiler}': {error}"),
        ),
        LinkFailureDetail::Status { stderr, status } => {
            if !stderr.is_empty() {
                if stderr.ends_with('\n') {
                    eprint!("{stderr}");
                } else {
                    eprintln!("{stderr}");
                }
            }
            eprintln!(
                "tea: C compiler '{compiler}' failed while linking '{}' ({status})",
                output.display()
            );
            CliError::silent(2)
        }
    }
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<(), CliError> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = fs::metadata(path).map_err(|error| {
        CliError::new(2, format!("tea: cannot stat '{}': {error}", path.display()))
    })?;
    let mut permissions = metadata.permissions();
    permissions.set_mode(permissions.mode() | 0o111);
    fs::set_permissions(path, permissions).map_err(|error| {
        CliError::new(
            2,
            format!("tea: cannot mark '{}' executable: {error}", path.display()),
        )
    })
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<(), CliError> {
    Ok(())
}
