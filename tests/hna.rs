//! End-to-end tests for HNA v1 (Hydro Native ABI) bindings.
//!
//! Fixtures:
//! - `crates/hydro-hna-fixture` (Rust `cdylib`, installed as `hna_rust.hna.so`)
//!   from the sibling hydro workspace,
//! - `tests/fixtures/hna_c.c` (C99, rebuilt as `hna_c.hna.so`),
//! - `tests/fixtures/hna_go/` (Go `-buildmode=c-shared`, `hna_go.hna.so`).
//!
//! The tests drive `tea infuse` and, when `libhydro_runtime.a` is available,
//! `tea build`. Hosts without `cc` skip the C-driven tests; hosts without
//! `go` skip the Go test; hosts without the Rust fixture skip cleanly with a
//! printed reason. All temporary directories are removed on drop.

use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::LazyLock;

#[cfg(target_os = "linux")]
use std::os::unix::process::ExitStatusExt;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Console output of the primary fixture program.
const MAIN_SOURCE: &str = "\
import hna_c
import hna_rust

print(hna_rust.add(2, 3))
print(hna_rust.MAGIC)
print(hna_rust.greet(\"hydro\"))
print(hna_rust.make_list(4))
print(hna_rust.add(a=2, b=3))
print(hna_c.mul(6, 7))
print(hna_c.tag())
print(hna_c.C)
";

const EXPECTED_STDOUT: &str = "\
5
7
hello, hydro
[0, 1, 2, 3]
5
42
C
C
";

/// Catches the documented call errors so the process still exits 0.
const ERROR_SOURCE: &str = "\
import hna_rust

print(hna_rust.add(a=2, b=3))

try:
    hna_rust.add(1)
except TypeError as error:
    print(\"arity TypeError \" + str(error))

try:
    hna_rust.add(1, 2, 3)
except TypeError as error:
    print(\"extra TypeError \" + str(error))

try:
    hna_rust.add(a=1, c=3)
except TypeError as error:
    print(\"keyword TypeError \" + str(error))

try:
    hna_rust.add(\"a\", 1)
except TypeError as error:
    print(\"type TypeError \" + str(error))

try:
    hna_rust.make_list(\"x\")
except TypeError as error:
    print(\"list TypeError \" + str(error))

try:
    hna_rust.fail()
except ValueError as error:
    print(\"failure ValueError \" + str(error))
";

/// Strict-GC stress: every iteration allocates through the binding.
const GC_SOURCE: &str = "\
import gc
import hna_rust

total = 0
for i in range(2000):
    values = hna_rust.make_list(8)
    total += values[7]
    text = hna_rust.greet(\"x\")
    if i % 100 == 0:
        gc.collect()

print(total)
print(len(hna_rust.make_list(3)))
";

const GC_EXPECTED: &str = "14000\n3\n";

struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new(test_name: &str) -> TestDir {
        let unique = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "tea-hna-{}-{test_name}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        TestDir { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn write(&self, relative: &str, contents: &str) {
        std::fs::write(self.path.join(relative), contents).expect("write test file");
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn hydro_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../hydro")
}

fn hna_include() -> PathBuf {
    hydro_root().join("include")
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn output_parts(output: Output) -> (i32, String, String) {
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn run_tea(dir: &Path, args: &[&str]) -> (i32, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_tea"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to spawn tea");
    output_parts(output)
}

fn run_tea_env(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> (i32, String, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tea"));
    command.args(args).current_dir(dir);
    for (key, value) in envs {
        command.env(key, value);
    }
    output_parts(command.output().expect("failed to spawn tea"))
}

fn run_executable(path: &Path, dir: &Path) -> (i32, String, String) {
    let output = Command::new(path)
        .current_dir(dir)
        .output()
        .expect("failed to spawn built executable");
    output_parts(output)
}

fn cc_available() -> bool {
    Command::new("cc")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn go_available() -> bool {
    Command::new("go")
        .arg("version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn runtime_lib_present() -> bool {
    let Ok(exe) = env::current_exe() else {
        return false;
    };
    exe.ancestors()
        .any(|dir| dir.join("libhydro_runtime.a").is_file())
}

/// Fails when `binary` dynamically links `libpython`; skipped when `ldd` is
/// unavailable.
#[cfg(target_os = "linux")]
fn assert_no_libpython(binary: &Path) {
    let output = match Command::new("ldd").arg(binary).output() {
        Ok(output) if output.status.success() => output,
        _ => {
            eprintln!(
                "skipping libpython check: `ldd` is unavailable for '{}'",
                binary.display()
            );
            return;
        }
    };
    let listing = String::from_utf8_lossy(&output.stdout);
    assert!(
        !listing.contains("libpython"),
        "'{}' links libpython:\n{listing}",
        binary.display()
    );
}

#[cfg(not(target_os = "linux"))]
fn assert_no_libpython(_binary: &Path) {}

/// Compiles `tests/fixtures/hna_c.c` as `<name>.hna.so` in `dir`.
fn compile_c(dir: &TestDir, name: &str, defines: &[&str]) -> PathBuf {
    let output = dir.path().join(format!("{name}.hna.so"));
    let source = fixtures_dir().join("hna_c.c");
    let mut command = Command::new("cc");
    command
        .arg("-std=c99")
        .arg("-shared")
        .arg("-fPIC")
        .arg("-I")
        .arg(hna_include())
        .arg("-o")
        .arg(&output)
        .arg(&source);
    for define in defines {
        command.arg(format!("-D{define}"));
    }
    let result = command.output().expect("failed to spawn cc");
    assert!(
        result.status.success(),
        "failed to compile the HNA C fixture:\n{}",
        String::from_utf8_lossy(&result.stderr)
    );
    output
}

fn target_fixture(dir: &Path) -> Option<PathBuf> {
    let direct = dir.join("libhydro_hna_fixture.so");
    if direct.is_file() {
        return Some(direct);
    }
    let deps = dir.join("deps");
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(deps)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                return false;
            };
            name.starts_with("libhydro_hna_fixture") && name.ends_with(".so")
        })
        .collect();
    candidates.sort();
    candidates.pop()
}

fn find_rust_fixture_in(target: &Path) -> Option<PathBuf> {
    for profile in ["debug", "release"] {
        if let Some(path) = target_fixture(&target.join(profile)) {
            return Some(path);
        }
    }
    None
}

/// Locates the Rust cdylib built by the hydro workspace, or builds it once
/// with `cargo build --offline`. Returns `None` (with a printed reason) when
/// neither is possible.
static RUST_FIXTURE: LazyLock<Option<PathBuf>> = LazyLock::new(locate_rust_fixture);

fn rust_fixture() -> Option<PathBuf> {
    RUST_FIXTURE.clone()
}

fn locate_rust_fixture() -> Option<PathBuf> {
    if let Some(path) = env::var_os("HYDRO_HNA_RUST_FIXTURE") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
        eprintln!("HYDRO_HNA_RUST_FIXTURE='{}' is not a file", path.display());
    }
    if let Some(target) = env::var_os("CARGO_TARGET_DIR") {
        if let Some(path) = find_rust_fixture_in(Path::new(&target)) {
            return Some(path);
        }
    }
    let default_target = hydro_root().join("target");
    if let Some(path) = find_rust_fixture_in(&default_target) {
        return Some(path);
    }
    if let Ok(exe) = env::current_exe() {
        for ancestor in exe.ancestors() {
            if let Some(path) = target_fixture(ancestor) {
                return Some(path);
            }
            if ancestor.join("libhydro_runtime.a").is_file() {
                for profile in ["debug", "release"] {
                    if let Some(path) = target_fixture(&ancestor.join(profile)) {
                        return Some(path);
                    }
                }
            }
        }
    }

    // A workspace build normally produced the cdylib already. Build it on
    // demand without touching the network.
    let manifest = hydro_root().join("Cargo.toml");
    if !manifest.is_file() {
        eprintln!(
            "skipping HNA Rust fixture: '{}' is missing",
            manifest.display()
        );
        return None;
    }
    let cargo = env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let build = Command::new(cargo)
        .arg("build")
        .arg("--manifest-path")
        .arg(&manifest)
        .arg("-p")
        .arg("hydro-hna-fixture")
        .arg("--offline")
        .arg("--quiet")
        .output();
    match build {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            eprintln!(
                "skipping HNA Rust fixture: cargo build failed:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return None;
        }
        Err(error) => {
            eprintln!("skipping HNA Rust fixture: cannot run cargo: {error}");
            return None;
        }
    }
    if let Some(path) = find_rust_fixture_in(&default_target) {
        return Some(path);
    }
    if let Some(target) = env::var_os("CARGO_TARGET_DIR") {
        if let Some(path) = find_rust_fixture_in(Path::new(&target)) {
            return Some(path);
        }
    }
    eprintln!(
        "skipping HNA Rust fixture: no libhydro_hna_fixture.so under '{}'",
        default_target.display()
    );
    None
}

/// Copies the workspace-built cdylib into `dir` under its canonical HNA name.
fn install_rust_fixture(dir: &TestDir) -> Option<PathBuf> {
    let source = rust_fixture()?;
    let target = dir.path().join("hna_rust.hna.so");
    std::fs::copy(&source, &target).expect("copy Rust HNA fixture");
    Some(target)
}

/// Builds `tests/fixtures/hna_go` with `-buildmode=c-shared`.
fn build_go(dir: &TestDir) -> PathBuf {
    let source = fixtures_dir().join("hna_go");
    let output = dir.path().join("hna_go.hna.so");
    let result = Command::new("go")
        .arg("build")
        .arg("-buildmode=c-shared")
        .arg("-o")
        .arg(&output)
        .arg(".")
        .current_dir(&source)
        .env("CGO_ENABLED", "1")
        .env("GOPROXY", "off")
        .env("GOTOOLCHAIN", "local")
        .output()
        .expect("failed to spawn go");
    assert!(
        result.status.success(),
        "failed to build the Go c-shared fixture:\n{}",
        String::from_utf8_lossy(&result.stderr)
    );
    output
}

/// Runs `main.py` under `tea build` and the resulting binary.
fn check_aot(dir: &TestDir, expected_stdout: &str) {
    if !runtime_lib_present() {
        eprintln!("skipping HNA AOT check: libhydro_runtime.a not present in the test profile");
        return;
    }
    let program = dir.path().join("program");
    let program_arg = program.to_string_lossy().into_owned();
    let (code, _stdout, stderr) = run_tea(
        dir.path(),
        &["build", "main.py", "-o", program_arg.as_str(), "--no-cache"],
    );
    assert_eq!(code, 0, "tea build failed:\n{stderr}");
    let (code, stdout, stderr) = run_executable(&program, dir.path());
    assert_eq!(code, 0, "AOT binary failed:\n{stderr}");
    assert_eq!(stdout, expected_stdout, "unexpected AOT output:\n{stderr}");
    assert_no_libpython(&program);
}

#[test]
fn rust_and_c_fixtures_run_jit_and_aot() {
    if !cc_available() {
        eprintln!("skipping HNA C fixture test: `cc` is not available");
        return;
    }
    let dir = TestDir::new("main");
    let Some(rust) = install_rust_fixture(&dir) else {
        return;
    };
    let c = compile_c(&dir, "hna_c", &[]);
    dir.write("main.py", MAIN_SOURCE);

    let (code, stdout, stderr) = run_tea(dir.path(), &["infuse", "main.py"]);
    assert_eq!(code, 0, "tea infuse failed:\n{stderr}");
    assert_eq!(stdout, EXPECTED_STDOUT, "unexpected JIT output:\n{stderr}");
    assert_no_libpython(Path::new(env!("CARGO_BIN_EXE_tea")));
    assert_no_libpython(&rust);
    assert_no_libpython(&c);

    check_aot(&dir, EXPECTED_STDOUT);
}

#[test]
fn keyword_calls_and_call_errors_use_python_exception_classes() {
    let dir = TestDir::new("errors");
    let Some(_rust) = install_rust_fixture(&dir) else {
        return;
    };
    dir.write("main.py", ERROR_SOURCE);

    let (code, stdout, stderr) = run_tea(dir.path(), &["infuse", "main.py"]);
    assert_eq!(code, 0, "tea infuse failed:\n{stderr}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 7, "unexpected output:\n{stdout}\n{stderr}");
    assert_eq!(lines[0], "5", "keyword call failed:\n{stdout}\n{stderr}");
    assert!(
        lines[1].starts_with("arity TypeError ") && lines[1].contains("add()"),
        "unexpected arity diagnostic: {}",
        lines[1]
    );
    let extra_arity =
        lines[2].contains("add() takes 2 positional argument") || lines[2].contains("at most 2");
    assert!(
        lines[2].starts_with("extra TypeError ") && extra_arity,
        "unexpected extra-argument diagnostic: {}",
        lines[2]
    );
    assert!(
        lines[3].starts_with("keyword TypeError ")
            && lines[3].contains("unexpected keyword argument 'c'"),
        "unexpected keyword diagnostic: {}",
        lines[3]
    );
    assert!(
        lines[4].starts_with("type TypeError ") && lines[4].contains("requires int arguments"),
        "unexpected type diagnostic: {}",
        lines[4]
    );
    assert!(
        lines[5].starts_with("list TypeError ") && lines[5].contains("requires an int argument"),
        "unexpected make_list diagnostic: {}",
        lines[5]
    );
    assert_eq!(
        lines[6], "failure ValueError hna_rust.fail() deliberate failure",
        "unexpected ValueError output"
    );

    check_aot(&dir, &stdout);
}

fn assert_import_failure(defines: &[&str], expected: &[&str]) {
    let dir = TestDir::new("reject");
    compile_c(&dir, "hna_c", defines);
    dir.write(
        "main.py",
        "import hna_c\nprint(hna_c.mul(1, 1))\nprint(\"unreachable\")\n",
    );
    let (code, stdout, stderr) = run_tea(dir.path(), &["infuse", "main.py"]);
    assert_ne!(code, 0, "mismatched descriptor must fail:\n{stderr}");
    assert!(
        !stdout.contains("unreachable"),
        "program ran with a rejected descriptor:\n{stdout}"
    );
    for needle in expected {
        assert!(
            stderr.contains(needle),
            "missing diagnostic {needle:?} in:\n{stderr}"
        );
    }
}

#[test]
fn descriptor_metadata_mismatches_fail_with_diagnostics() {
    if !cc_available() {
        eprintln!("skipping HNA rejection tests: `cc` is not available");
        return;
    }

    assert_import_failure(
        &["HNA_FIXTURE_HNA_VERSION=2"],
        &[
            "hna module 'hna_c'",
            "descriptor hna_version 2, runtime supports 1",
        ],
    );
    assert_import_failure(
        &["HNA_FIXTURE_RUNTIME_ABI_MIN=2"],
        &["descriptor runtime_abi_min 2, runtime supports 1"],
    );
    assert_import_failure(
        &["HNA_FIXTURE_PLATFORM=\"linux-aarch64\""],
        &["descriptor platform 'linux-aarch64', runtime platform 'linux-x86_64'"],
    );
    assert_import_failure(
        &["HNA_FIXTURE_MODULE_NAME=\"hna_wrong\""],
        &["descriptor module_name 'hna_wrong' does not match import name 'hna_c'"],
    );
}

#[cfg(target_os = "linux")]
#[test]
fn aborted_init_terminates_process_instead_of_raising() {
    if !cc_available() {
        eprintln!("skipping HNA abort test: `cc` is not available");
        return;
    }
    let dir = TestDir::new("abort");
    compile_c(&dir, "hna_c", &["HNA_FIXTURE_ABORT=1"]);
    dir.write(
        "main.py",
        "import hna_c\nprint(hna_c.mul(1, 1))\nprint(\"after import\")\n",
    );

    let output = Command::new(env!("CARGO_BIN_EXE_tea"))
        .args(["infuse", "main.py"])
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn tea");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !output.status.success(),
        "aborting init must not succeed:\nstdout:\n{stdout}"
    );
    assert!(
        output.status.code().is_none(),
        "aborting init must die by signal, not exit {}",
        output.status.code().unwrap_or(-1)
    );
    assert_eq!(
        output.status.signal(),
        Some(6),
        "expected SIGABRT, got {:?}",
        output.status.signal()
    );
    assert!(
        !stdout.contains("after import"),
        "import continued after abort:\n{stdout}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn panicking_binding_aborts_without_unwinding() {
    let dir = TestDir::new("panic");
    let Some(_rust) = install_rust_fixture(&dir) else {
        return;
    };
    dir.write(
        "main.py",
        "import hna_rust\nprint(hna_rust.boom())\nprint(\"after boom\")\n",
    );

    let output = Command::new(env!("CARGO_BIN_EXE_tea"))
        .args(["infuse", "main.py"])
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn tea");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "panicking binding must not exit normally:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        output.status.code().is_none(),
        "panicking binding must die by signal, not exit {}",
        output.status.code().unwrap_or(-1)
    );
    assert_eq!(
        output.status.signal(),
        Some(6),
        "expected SIGABRT, got {:?}\nstderr:\n{stderr}",
        output.status.signal()
    );
    assert!(
        stdout.is_empty(),
        "program continued after the panic:\n{stdout}"
    );
    let diagnostic = stderr.to_lowercase();
    assert!(
        diagnostic.contains("panic") || diagnostic.contains("abort"),
        "missing panic/abort diagnostic:\n{stderr}"
    );
}

#[test]
fn go_c_shared_fixture_runs_jit_and_aot() {
    if !go_available() {
        eprintln!("skipping HNA Go fixture test: `go` is not available");
        return;
    }
    let dir = TestDir::new("go");
    let artifact = build_go(&dir);
    dir.write(
        "main.py",
        "import hna_go\nprint(hna_go.add(2, 3))\nprint(hna_go.add(a=2, b=3))\n",
    );

    let (code, stdout, stderr) = run_tea(dir.path(), &["infuse", "main.py"]);
    assert_eq!(code, 0, "tea infuse failed:\n{stderr}");
    assert_eq!(stdout, "5\n5\n", "unexpected Go fixture output:\n{stderr}");
    assert_no_libpython(&artifact);

    check_aot(&dir, "5\n5\n");
}

#[test]
fn precise_gc_repeated_binding_calls_stay_rooted() {
    let dir = TestDir::new("gc");
    let Some(_rust) = install_rust_fixture(&dir) else {
        return;
    };
    dir.write("main.py", GC_SOURCE);

    let (code, stdout, stderr) = run_tea_env(
        dir.path(),
        &["infuse", "main.py"],
        &[("HYDRO_GC_PRECISE", "1")],
    );
    assert_eq!(code, 0, "strict-GC run failed:\n{stderr}");
    assert_eq!(
        stdout, GC_EXPECTED,
        "unexpected strict-GC output:\n{stderr}"
    );
}
