//! NB-3 end-to-end pilots for native bindings.
//!
//! - **Generated libm binding**: `hydro-bindgen` turns the real-libm fixture
//!   (`crates/hydro-bindgen/tests/fixtures/libm_real.h`) into a standalone
//!   crate, that crate is built offline, published as `hna_libm.hna.so`, and
//!   its `sqrt`/`hypot` results are compared against CPython's `math` under
//!   `tea infuse` and `tea build`. Keyword calls and call errors are checked
//!   too, and `hydro-bindgen check` must reproduce the generated tree
//!   byte-for-byte (a mutated file must fail the check).
//! - **Handwritten libsqlite3 binding**: the in-tree `hydro-sqlite-hna`
//!   cdylib is built, published as `hna_sqlite.hna.so`, and driven through a
//!   table/insert/rowid/close program whose scalar output must equal a CPython
//!   `sqlite3` oracle.
//!
//! Both pilots assert their artifacts do not link `libpython`. Hosts without
//! the `hydro-bindgen` binary, `cc` + SQLite development headers, `python3`,
//! or `libhydro_runtime.a` skip the affected checks with a printed reason.
//! All temporary directories are removed on drop.

use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::LazyLock;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

const LIBM_SOURCE: &str = "\
import hna_libm

print(hna_libm.sqrt(9.0))
print(hna_libm.hypot(3.0, 4.0))
print(hna_libm.sqrt(x=16.0))
print(hna_libm.hypot(y=12.0, x=5.0))
";

const LIBM_ORACLE: &str = "\
import math

print(math.sqrt(9.0))
print(math.hypot(3.0, 4.0))
print(math.sqrt(16.0))
print(math.hypot(5.0, 12.0))
";

const LIBM_ERRORS: &str = "\
import hna_libm

try:
    hna_libm.sqrt(1.0, 2.0)
except TypeError as error:
    print(\"arity TypeError \" + str(error))

try:
    hna_libm.hypot(3.0)
except TypeError as error:
    print(\"missing TypeError \" + str(error))

try:
    hna_libm.sqrt(\"nine\")
except TypeError as error:
    print(\"type TypeError \" + str(error))

try:
    hna_libm.hypot(x=\"bad\", y=4.0)
except TypeError as error:
    print(\"keyword TypeError \" + str(error))
";

const SQLITE_SOURCE: &str = "\
import hna_sqlite

db = hna_sqlite.open(\"hydro-pilot.db\")
hna_sqlite.exec(db, \"CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)\")
names = [\"alpha\", \"beta\", \"gamma\"]
for name in names:
    hna_sqlite.exec(db, \"INSERT INTO t (name) VALUES ('\" + name + \"')\")
    print(hna_sqlite.changes(db))
print(hna_sqlite.total_changes(db))
print(hna_sqlite.last_insert_rowid(db))
hna_sqlite.close(db)
";

const SQLITE_ORACLE: &str = "\
import sqlite3

connection = sqlite3.connect(\"cpython-pilot.db\")
cursor = connection.cursor()
cursor.execute(\"CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)\")
for name in (\"alpha\", \"beta\", \"gamma\"):
    cursor.execute(\"INSERT INTO t (name) VALUES (?)\", (name,))
    print(cursor.rowcount)
print(connection.total_changes)
print(cursor.lastrowid)
connection.close()
";

const SQLITE_ERRORS: &str = "\
import hna_sqlite

db = hna_sqlite.open(path=\"hydro-errors.db\")
print(hna_sqlite.exec(db, \"CREATE TABLE IF NOT EXISTS probe (id INTEGER)\"))

try:
    hna_sqlite.exec(db, \"THIS IS NOT SQL\")
except ValueError as error:
    print(\"sql ValueError\")

try:
    hna_sqlite.exec(\"not-a-handle\", \"SELECT 1\")
except TypeError as error:
    print(\"handle TypeError\")

try:
    hna_sqlite.changes(424242)
except ValueError as error:
    print(\"handle ValueError\")

print(hna_sqlite.close(db))

try:
    hna_sqlite.close(db)
except ValueError as error:
    print(\"closed ValueError\")
";

struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new(test_name: &str) -> TestDir {
        let unique = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "tea-bindgen-{}-{test_name}-{unique}",
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

fn hna_crate() -> PathBuf {
    let path = hydro_root().join("crates/hydro-hna");
    path.canonicalize().unwrap_or(path)
}

fn libm_header() -> PathBuf {
    hydro_root().join("crates/hydro-bindgen/tests/fixtures/libm_real.h")
}

fn libm_descriptor_fixture() -> PathBuf {
    hydro_root().join("crates/hydro-bindgen/tests/fixtures/libm_real.descriptor.expected")
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

fn run_executable(path: &Path, dir: &Path) -> (i32, String, String) {
    let output = Command::new(path)
        .current_dir(dir)
        .output()
        .expect("failed to spawn built executable");
    output_parts(output)
}

fn run_python(dir: &Path, script: &str) -> (i32, String, String) {
    let output = Command::new("python3")
        .arg(script)
        .current_dir(dir)
        .output()
        .expect("failed to spawn python3");
    output_parts(output)
}

fn cc_available() -> bool {
    Command::new("cc")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn python3_available() -> bool {
    Command::new("python3")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn cargo_command() -> OsString {
    env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"))
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

/// Cargo target roots that may hold a cdylib built from the hydro workspace.
fn target_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(target) = env::var_os("CARGO_TARGET_DIR") {
        roots.push(PathBuf::from(target));
    }
    roots.push(hydro_root().join("target"));
    if let Ok(exe) = env::current_exe() {
        for ancestor in exe.ancestors() {
            let looks_like_target = ancestor.file_name() == Some(std::ffi::OsStr::new("target"));
            if looks_like_target || ancestor.join("libhydro_runtime.a").is_file() {
                roots.push(ancestor.to_path_buf());
            }
        }
    }
    roots
}

fn find_in_target(target: &Path, crate_name: &str) -> Option<PathBuf> {
    let file = format!(
        "{}{}{}",
        env::consts::DLL_PREFIX,
        crate_name,
        env::consts::DLL_SUFFIX
    );
    for profile in ["debug", "release"] {
        let candidate = target.join(profile).join(&file);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn find_cdylib(crate_name: &str) -> Option<PathBuf> {
    target_roots()
        .into_iter()
        .find_map(|root| find_in_target(&root, crate_name))
}

/// Locates the `hydro-bindgen` binary: `HYDRO_BINDGEN` override, then a
/// sibling hydro target directory, then an offline build of the crate.
static BINDGEN: LazyLock<Option<PathBuf>> = LazyLock::new(locate_bindgen);

fn bindgen_binary() -> Option<PathBuf> {
    BINDGEN.clone()
}

fn locate_bindgen() -> Option<PathBuf> {
    if let Some(path) = env::var_os("HYDRO_BINDGEN") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
        eprintln!("HYDRO_BINDGEN='{}' is not a file", path.display());
    }

    let exe = format!("hydro-bindgen{}", env::consts::EXE_SUFFIX);
    let manifest = hydro_root().join("Cargo.toml");
    let mut fresh = false;
    if manifest.is_file() {
        // Refresh before searching so a binary left over from an older source
        // tree is never exercised. Cargo is a no-op when it is current.
        let build = Command::new(cargo_command())
            .arg("build")
            .arg("--manifest-path")
            .arg(&manifest)
            .arg("-p")
            .arg("hydro-bindgen")
            .arg("--offline")
            .arg("--quiet")
            .output();
        match build {
            Ok(output) if output.status.success() => fresh = true,
            Ok(output) => eprintln!(
                "warning: cargo build -p hydro-bindgen failed; using an existing binary if present:\n{}",
                String::from_utf8_lossy(&output.stderr)
            ),
            Err(error) => {
                eprintln!("warning: cannot run cargo to refresh hydro-bindgen: {error}")
            }
        }
    }

    for root in target_roots() {
        for profile in ["debug", "release"] {
            let candidate = root.join(profile).join(&exe);
            if candidate.is_file() {
                if !fresh {
                    eprintln!(
                        "warning: using an unrefreshed hydro-bindgen: '{}'",
                        candidate.display()
                    );
                }
                return Some(candidate);
            }
        }
    }
    eprintln!("skipping generated-binding pilot: hydro-bindgen is not available");
    None
}

fn run_bindgen(args: &[&str]) -> (i32, String, String) {
    let binary = bindgen_binary().expect("hydro-bindgen must be located first");
    let output = Command::new(binary)
        .args(args)
        .output()
        .expect("failed to spawn hydro-bindgen");
    output_parts(output)
}

/// `true` when `cc` can preprocess `<sqlite3.h>` (headers installed).
fn sqlite_dev_available(dir: &TestDir) -> bool {
    if !cc_available() {
        return false;
    }
    let probe = dir.path().join("sqlite_probe.c");
    std::fs::write(&probe, "#include <sqlite3.h>\n").expect("write sqlite probe");
    Command::new("cc")
        .arg("-E")
        .arg(&probe)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Builds the generated crate in `dir` offline with a private target dir and
/// returns the unhashed cdylib path.
fn build_generated_crate(dir: &TestDir, crate_dir: &Path) -> PathBuf {
    let target = dir.path().join("target");
    let output = Command::new(cargo_command())
        .arg("build")
        .arg("--offline")
        .current_dir(crate_dir)
        .env("CARGO_TARGET_DIR", &target)
        .output()
        .expect("failed to spawn cargo for the generated binding");
    assert!(
        output.status.success(),
        "cargo build failed for the generated binding:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    find_in_target(&target, "hydro_binding_hna_libm").unwrap_or_else(|| {
        panic!(
            "generated binding cdylib not found under '{}'",
            target.display()
        )
    })
}

/// Builds the in-tree sqlite HNA crate offline and returns its cdylib path.
fn build_sqlite_crate() -> Option<PathBuf> {
    let manifest = hydro_root().join("Cargo.toml");
    if !manifest.is_file() {
        eprintln!("skipping sqlite pilot: '{}' is missing", manifest.display());
        return None;
    }
    let output = Command::new(cargo_command())
        .arg("build")
        .arg("--manifest-path")
        .arg(&manifest)
        .arg("-p")
        .arg("hydro-sqlite-hna")
        .arg("--offline")
        .arg("--quiet")
        .output()
        .expect("failed to spawn cargo for hydro-sqlite-hna");
    assert!(
        output.status.success(),
        "cargo build -p hydro-sqlite-hna failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    match find_cdylib("hydro_sqlite_hna") {
        Some(path) => Some(path),
        None => {
            eprintln!("skipping sqlite pilot: no libhydro_sqlite_hna cdylib under the target dirs");
            None
        }
    }
}

/// Runs `main.py` under `tea build` and the resulting binary.
fn check_aot(dir: &TestDir, source: &str, program_name: &str, expected_stdout: &str) {
    if !runtime_lib_present() {
        eprintln!("skipping bindgen AOT check: libhydro_runtime.a not present in the test profile");
        return;
    }
    let program = dir.path().join(program_name);
    let program_arg = program.to_string_lossy().into_owned();
    let (code, _stdout, stderr) = run_tea(
        dir.path(),
        &["build", source, "-o", program_arg.as_str(), "--no-cache"],
    );
    assert_eq!(code, 0, "tea build failed:\n{stderr}");
    let (code, stdout, stderr) = run_executable(&program, dir.path());
    assert_eq!(code, 0, "AOT binary failed:\n{stderr}");
    assert_eq!(stdout, expected_stdout, "unexpected AOT output:\n{stderr}");
    assert_no_libpython(&program);
}

/// Asserts that each non-comment line of the checked-in descriptor fragment
/// appears in the generated `src/lib.rs`.
fn assert_descriptor_fragment(generated: &str) {
    let fixture = libm_descriptor_fixture();
    let expected = std::fs::read_to_string(&fixture)
        .unwrap_or_else(|error| panic!("cannot read '{}': {error}", fixture.display()));
    for line in expected.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        assert!(
            generated.contains(line),
            "generated descriptor is missing fixture line:\n{line}"
        );
    }
}

fn assert_error_line(line: &str, prefix: &str) {
    assert!(
        line.starts_with(prefix),
        "expected an error line starting with {prefix:?}, got {line:?}"
    );
}

#[test]
fn generated_libm_binding_matches_cpython_under_jit_and_aot() {
    if !python3_available() {
        eprintln!("skipping generated-binding pilot: `python3` is not available");
        return;
    }
    let Some(_bindgen) = bindgen_binary() else {
        return;
    };
    let header = libm_header();
    assert!(header.is_file(), "'{}' is missing", header.display());

    let dir = TestDir::new("libm");
    let generated = dir.path().join("hna_libm");
    let header_arg = header.to_string_lossy().into_owned();
    let crate_arg = hna_crate().to_string_lossy().into_owned();
    let out_arg = generated.to_string_lossy().into_owned();

    let (code, stdout, stderr) = run_bindgen(&[
        "generate",
        "--header",
        header_arg.as_str(),
        "--module",
        "hna_libm",
        "--provider",
        "libm fixture",
        "--library",
        "m",
        "--hna-crate",
        crate_arg.as_str(),
        "--out",
        out_arg.as_str(),
    ]);
    assert_eq!(code, 0, "hydro-bindgen generate failed:\n{stderr}");
    assert!(
        stdout.contains("generated module `hna_libm`"),
        "unexpected generate output:\n{stdout}"
    );

    let lib_rs =
        std::fs::read_to_string(generated.join("src/lib.rs")).expect("read generated src/lib.rs");
    assert_descriptor_fragment(&lib_rs);

    let artifact = build_generated_crate(&dir, &generated);
    let installed = dir.path().join("hna_libm.hna.so");
    std::fs::copy(&artifact, &installed).expect("publish hna_libm.hna.so");
    assert_no_libpython(&installed);
    assert_no_libpython(Path::new(env!("CARGO_BIN_EXE_tea")));

    dir.write("main.py", LIBM_SOURCE);
    dir.write("oracle.py", LIBM_ORACLE);
    let (code, oracle_stdout, oracle_stderr) = run_python(dir.path(), "oracle.py");
    assert_eq!(code, 0, "CPython oracle failed:\n{oracle_stderr}");
    assert!(
        !oracle_stdout.is_empty(),
        "CPython oracle produced no output"
    );

    let (code, stdout, stderr) = run_tea(dir.path(), &["infuse", "main.py"]);
    assert_eq!(code, 0, "tea infuse failed:\n{stderr}");
    assert_eq!(
        stdout, oracle_stdout,
        "generated libm output differs from CPython math:\n{stderr}"
    );
    check_aot(&dir, "main.py", "libm-main", &oracle_stdout);

    dir.write("errors.py", LIBM_ERRORS);
    let (code, stdout, stderr) = run_tea(dir.path(), &["infuse", "errors.py"]);
    assert_eq!(code, 0, "tea infuse errors.py failed:\n{stderr}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines.len(),
        4,
        "unexpected error output:\n{stdout}\n{stderr}"
    );
    assert_error_line(lines[0], "arity TypeError ");
    assert_error_line(lines[1], "missing TypeError ");
    assert_error_line(lines[2], "type TypeError ");
    assert_error_line(lines[3], "keyword TypeError ");
    check_aot(&dir, "errors.py", "libm-errors", &stdout);

    // `--check` must reproduce the tree, and must notice a mutation.
    let check_args = vec![
        "check",
        "--header",
        header_arg.as_str(),
        "--module",
        "hna_libm",
        "--provider",
        "libm fixture",
        "--library",
        "m",
        "--hna-crate",
        crate_arg.as_str(),
        "--out",
        out_arg.as_str(),
    ];
    let (code, stdout, stderr) = run_bindgen(&check_args);
    assert_eq!(
        code, 0,
        "hydro-bindgen check failed on a fresh tree:\n{stdout}\n{stderr}"
    );

    let mut mutated =
        std::fs::read_to_string(generated.join("src/lib.rs")).expect("read generated src/lib.rs");
    mutated.push_str("\n// mutated by tea/tests/bindgen.rs\n");
    std::fs::write(generated.join("src/lib.rs"), mutated).expect("mutate generated src/lib.rs");
    let (code, _stdout, stderr) = run_bindgen(&check_args);
    assert_ne!(
        code, 0,
        "hydro-bindgen check accepted a mutated generated file:\n{stderr}"
    );
}

#[test]
fn handwritten_sqlite_hna_matches_cpython_under_jit_and_aot() {
    if !cfg!(target_os = "linux") || env::consts::ARCH != "x86_64" {
        eprintln!("skipping sqlite pilot: the handwritten descriptor is linux-x86_64 only");
        return;
    }
    if !python3_available() {
        eprintln!("skipping sqlite pilot: `python3` is not available");
        return;
    }
    let dir = TestDir::new("sqlite");
    if !sqlite_dev_available(&dir) {
        eprintln!("skipping sqlite pilot: `cc` or the SQLite development headers are unavailable");
        return;
    }
    let Some(artifact) = build_sqlite_crate() else {
        return;
    };
    let installed = dir.path().join("hna_sqlite.hna.so");
    std::fs::copy(&artifact, &installed).expect("publish hna_sqlite.hna.so");
    assert_no_libpython(&installed);
    assert_no_libpython(Path::new(env!("CARGO_BIN_EXE_tea")));

    dir.write("main.py", SQLITE_SOURCE);
    dir.write("oracle.py", SQLITE_ORACLE);
    let (code, oracle_stdout, oracle_stderr) = run_python(dir.path(), "oracle.py");
    assert_eq!(code, 0, "CPython sqlite3 oracle failed:\n{oracle_stderr}");
    assert!(
        !oracle_stdout.is_empty(),
        "CPython sqlite3 oracle produced no output"
    );

    let (code, stdout, stderr) = run_tea(dir.path(), &["infuse", "main.py"]);
    assert_eq!(code, 0, "tea infuse failed:\n{stderr}");
    assert_eq!(
        stdout, oracle_stdout,
        "hna_sqlite output differs from CPython sqlite3:\n{stderr}"
    );

    // The AOT binary runs the same program again, so start from a fresh file.
    let _ = std::fs::remove_file(dir.path().join("hydro-pilot.db"));
    check_aot(&dir, "main.py", "sqlite-main", &oracle_stdout);

    dir.write("errors.py", SQLITE_ERRORS);
    let (code, stdout, stderr) = run_tea(dir.path(), &["infuse", "errors.py"]);
    assert_eq!(code, 0, "tea infuse errors.py failed:\n{stderr}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines.len(),
        6,
        "unexpected error output:\n{stdout}\n{stderr}"
    );
    assert_eq!(
        lines[0], "None",
        "exec() must return None:\n{stdout}\n{stderr}"
    );
    assert_error_line(lines[1], "sql ValueError");
    assert_error_line(lines[2], "handle TypeError");
    assert_error_line(lines[3], "handle ValueError");
    assert_eq!(
        lines[4], "None",
        "close() must return None:\n{stdout}\n{stderr}"
    );
    assert_error_line(lines[5], "closed ValueError");
    check_aot(&dir, "errors.py", "sqlite-errors", &stdout);
}
