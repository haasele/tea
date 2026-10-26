//! Tests for the Tea toolchain scheme: `run` (uv compatibility), `infuse`
//! (Hydro JIT), `steep`/`flavors` (profiles) and `brew` (AOT with a flavor).
//!
//! Only the standard library is used; the binary is driven through
//! `CARGO_BIN_EXE_tea`.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// An isolated directory for a single test, removed on drop.
struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new(test_name: &str) -> Self {
        let unique = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "tea-flavors-{}-{test_name}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("failed to create test temp dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn write(&self, name: &str, source: &str) -> PathBuf {
        let path = self.path.join(name);
        std::fs::write(&path, source).expect("failed to write source");
        path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn output_parts(output: Output) -> (i32, String, String) {
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn run_in(dir: &Path, args: &[&str]) -> (i32, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_tea"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to spawn tea binary");
    output_parts(output)
}

fn run_in_with_env(dir: &Path, args: &[&str], envs: &[(&str, &OsStr)]) -> (i32, String, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tea"));
    command.args(args).current_dir(dir);
    for (key, value) in envs {
        command.env(key, value);
    }
    let output = command.output().expect("failed to spawn tea binary");
    output_parts(output)
}

fn read_flavor(dir: &Path, name: &str) -> String {
    let path = dir.join(".tea/flavors").join(format!("{name}.json"));
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read '{}': {error}", path.display()))
}

fn flavor_runs(text: &str) -> u64 {
    let marker = "\"runs\":";
    let start = text.find(marker).expect("missing runs") + marker.len();
    let digits: String = text[start..]
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .collect();
    digits.parse().expect("runs must be a number")
}

fn flavor_functions(text: &str) -> BTreeMap<String, u64> {
    let marker = "\"functions\":{";
    let start = text.find(marker).expect("missing functions") + marker.len();
    // Skip the object's own closing brace and the functions object's brace.
    let outer = text.rfind('}').expect("missing closing brace");
    let end = text[..outer].rfind('}').expect("missing functions brace");
    let inner = &text[start..end];
    inner
        .split(',')
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            let (name, count) = entry.split_once(':').expect("function entry");
            (
                name.trim_matches('"').to_string(),
                count.parse().expect("function count"),
            )
        })
        .collect()
}

fn total_count(functions: &BTreeMap<String, u64>) -> u64 {
    functions.values().sum()
}

/// A shell script standing in for `uv`.
struct FakeUv {
    program: PathBuf,
    log: PathBuf,
}

impl FakeUv {
    fn new(dir: &Path) -> Self {
        let program = dir.join("fake-uv");
        let log = dir.join("uv.log");
        std::fs::write(
            &program,
            "#!/bin/sh\n\
             {\n\
               printf 'cwd=%s\\n' \"$(pwd -P)\"\n\
               for arg in \"$@\"; do\n\
                 printf 'arg=%s\\n' \"$arg\"\n\
               done\n\
             } >> \"$TEA_FAKE_UV_LOG\"\n\
             exit \"${TEA_FAKE_UV_EXIT:-0}\"\n",
        )
        .expect("failed to write fake uv");
        let mut permissions = std::fs::metadata(&program)
            .expect("failed to stat fake uv")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&program, permissions).expect("failed to make fake uv executable");
        Self { program, log }
    }

    fn run(&self, dir: &Path, args: &[&str], exit: &str) -> (i32, Vec<String>) {
        let _ = std::fs::remove_file(&self.log);
        let (code, _stdout, _stderr) = run_in_with_env(
            dir,
            args,
            &[
                ("TEA_UV", self.program.as_os_str()),
                ("TEA_FAKE_UV_LOG", self.log.as_os_str()),
                ("TEA_FAKE_UV_EXIT", OsStr::new(exit)),
            ],
        );
        let recorded = std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect();
        (code, recorded)
    }
}

#[test]
fn run_delegates_to_uv_and_returns_its_status() {
    let dir = TestDir::new("run-uv");
    let fake = FakeUv::new(dir.path());
    let cwd = format!(
        "cwd={}",
        dir.path()
            .canonicalize()
            .expect("canonical temp dir")
            .display()
    );

    let (code, recorded) = fake.run(dir.path(), &["run", "probe.py", "--flag", "value"], "7");
    assert_eq!(code, 7, "tea run must return uv's exit status");
    assert_eq!(
        recorded,
        vec![
            cwd.clone(),
            "arg=run".to_owned(),
            "arg=probe.py".to_owned(),
            "arg=--flag".to_owned(),
            "arg=value".to_owned(),
        ],
        "tea run must forward its arguments to uv verbatim"
    );

    // Without a source file the arguments still reach `uv run`.
    let (code, recorded) = fake.run(dir.path(), &["run"], "0");
    assert_eq!(code, 0);
    assert_eq!(recorded, vec![cwd, "arg=run".to_owned()]);
}

#[test]
fn run_does_not_invoke_the_hydro_engine() {
    let dir = TestDir::new("run-no-engine");
    let fake = FakeUv::new(dir.path());
    // An intentionally invalid Hydro program: `tea run` must never compile it.
    let program = dir.write("broken.py", "this is not python\n");

    let (code, _recorded) = fake.run(
        dir.path(),
        &["run", program.to_str().expect("UTF-8 path")],
        "0",
    );
    assert_eq!(code, 0, "only uv decides whether the command succeeds");
}

#[test]
fn infuse_forwards_arguments_to_the_program() {
    let dir = TestDir::new("infuse-argv");
    let program = dir.write(
        "probe.py",
        "import sys\nprint(len(sys.argv))\nprint(sys.argv[1])\nprint(sys.argv[2])\n",
    );

    let (code, stdout, stderr) = run_in(
        dir.path(),
        &[
            "infuse",
            program.to_str().expect("UTF-8 path"),
            "--flag",
            "value",
        ],
    );
    assert_eq!(code, 0, "infuse failed:\n{stderr}");
    assert_eq!(stdout, "3\n--flag\nvalue\n");
}

#[test]
fn infuse_interprets_with_the_same_argv() {
    let dir = TestDir::new("infuse-interpret");
    let program = dir.write(
        "probe.py",
        "import sys\nprint(len(sys.argv))\nprint(sys.argv[1])\n",
    );

    let (code, stdout, stderr) = run_in(
        dir.path(),
        &[
            "infuse",
            "--interpret",
            program.to_str().expect("UTF-8 path"),
            "hello",
        ],
    );
    assert_eq!(code, 0, "interpreted infuse failed:\n{stderr}");
    assert_eq!(stdout, "2\nhello\n");
}

const HOT_PROGRAM: &str = "def hot():\n    return 1\n\nhot()\nhot()\nprint(\"done\")\n";

#[test]
fn steep_records_a_flavor_and_accumulates_runs() {
    let dir = TestDir::new("steep-accumulate");
    let program = dir.write("hot.py", HOT_PROGRAM);
    let path = program.to_str().expect("UTF-8 path");

    let (code, stdout, stderr) = run_in(dir.path(), &["steep", "--flavor", "hot", path]);
    assert_eq!(code, 0, "first steep failed:\n{stderr}");
    assert_eq!(stdout, "done\n", "steep must not pollute program stdout");
    assert!(
        stderr.contains("flavor 'hot'") && stderr.contains("runs"),
        "missing summary in:\n{stderr}"
    );

    let first = read_flavor(dir.path(), "hot");
    assert!(first.contains("\"version\":1"), "{first}");
    assert!(first.contains("\"flavor\":\"hot\""), "{first}");
    assert_eq!(flavor_runs(&first), 1);
    let first_functions = flavor_functions(&first);
    assert!(
        !first_functions.is_empty(),
        "a run must record at least the module body"
    );

    let (code, _stdout, stderr) = run_in(dir.path(), &["steep", "--flavor", "hot", path]);
    assert_eq!(code, 0, "second steep failed:\n{stderr}");
    let second = read_flavor(dir.path(), "hot");
    assert_eq!(flavor_runs(&second), 2);
    let second_functions = flavor_functions(&second);
    assert!(
        total_count(&second_functions) > total_count(&first_functions),
        "counts must increase across runs: {first_functions:?} -> {second_functions:?}"
    );
}

#[test]
fn steep_merges_existing_flavors_without_running() {
    let dir = TestDir::new("steep-merge");
    let program = dir.write("hot.py", HOT_PROGRAM);
    let path = program.to_str().expect("UTF-8 path");

    assert_eq!(run_in(dir.path(), &["steep", "--flavor", "one", path]).0, 0);
    assert_eq!(run_in(dir.path(), &["steep", "--flavor", "two", path]).0, 0);

    let (code, stdout, stderr) = run_in(
        dir.path(),
        &[
            "steep", "--flavor", "merged", "--merge", "one", "--merge", "two",
        ],
    );
    assert_eq!(code, 0, "merge failed:\n{stderr}");
    assert!(stdout.is_empty(), "merge-only steep must not touch stdout");
    assert!(stderr.contains("flavor 'merged'"), "{stderr}");

    let one = read_flavor(dir.path(), "one");
    let two = read_flavor(dir.path(), "two");
    let merged = read_flavor(dir.path(), "merged");
    assert_eq!(
        flavor_runs(&merged),
        flavor_runs(&one) + flavor_runs(&two),
        "runs must sum"
    );
    let mut expected = flavor_functions(&one);
    for (name, count) in flavor_functions(&two) {
        *expected.entry(name).or_insert(0) += count;
    }
    assert_eq!(
        flavor_functions(&merged),
        expected,
        "function counts must sum"
    );
}

#[test]
fn steep_rejects_invalid_flavor_names() {
    let dir = TestDir::new("steep-invalid");
    let program = dir.write("hot.py", "print(\"x\")\n");
    let path = program.to_str().expect("UTF-8 path");

    for name in ["../escape", "bad name", ""] {
        let (code, _stdout, stderr) = run_in(dir.path(), &["steep", "--flavor", name, path]);
        assert_eq!(code, 2, "invalid name '{name}' must be a usage error");
        assert!(
            stderr.contains("invalid flavor name"),
            "missing validation message for '{name}':\n{stderr}"
        );
    }
    assert!(
        !dir.path().join(".tea").join("escape.json").exists(),
        "an invalid name must never write outside .tea/flavors"
    );
}

#[test]
fn brew_rejects_a_missing_flavor() {
    let dir = TestDir::new("brew-missing");
    let program = dir.write("hello.py", "print(\"hi\")\n");

    let (code, _stdout, stderr) = run_in(
        dir.path(),
        &[
            "brew",
            program.to_str().expect("UTF-8 path"),
            "--flavor",
            "nope",
        ],
    );
    assert_ne!(code, 0, "a missing flavor must fail");
    assert!(
        stderr.contains("flavor 'nope'") && stderr.contains("not found"),
        "missing actionable error:\n{stderr}"
    );
}

#[test]
fn flavors_lists_stored_profiles() {
    let dir = TestDir::new("flavors-list");
    let (code, stdout, _stderr) = run_in(dir.path(), &["flavors"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("no flavors stored"), "{stdout}");

    let program = dir.write("hot.py", HOT_PROGRAM);
    assert_eq!(
        run_in(
            dir.path(),
            &[
                "steep",
                "--flavor",
                "bench",
                program.to_str().expect("UTF-8 path")
            ]
        )
        .0,
        0
    );

    let (code, stdout, stderr) = run_in(dir.path(), &["flavors"]);
    assert_eq!(code, 0, "flavors failed:\n{stderr}");
    assert!(stdout.contains("bench"), "{stdout}");
    assert!(stdout.contains("runs"), "{stdout}");
    assert!(stdout.contains("functions"), "{stdout}");
}

/// Whether the AOT runtime archive is available next to the test profile.
fn runtime_lib_present() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    exe.ancestors()
        .any(|dir| dir.join("libhydro_runtime.a").is_file())
}

fn run_executable(path: &Path) -> (i32, String, String) {
    let output = Command::new(path)
        .output()
        .expect("failed to spawn built executable");
    output_parts(output)
}

#[test]
fn brew_with_flavor_builds_a_runnable_binary() {
    if !runtime_lib_present() {
        eprintln!(
            "skipping brew_with_flavor_builds_a_runnable_binary: libhydro_runtime.a not found"
        );
        return;
    }

    let dir = TestDir::new("brew-flavor");
    let program = dir.write("hello.py", "print(\"Hello, brew!\")\n");
    let path = program.to_str().expect("UTF-8 path");

    let (code, _stdout, stderr) = run_in(dir.path(), &["steep", "--flavor", "default", path]);
    assert_eq!(code, 0, "steep failed:\n{stderr}");

    let binary = dir.path().join("hello-flavored");
    let binary_arg = binary.to_str().expect("UTF-8 path");
    let (code, stdout, stderr) = run_in(
        dir.path(),
        &["brew", path, "--flavor", "default", "-o", binary_arg],
    );
    assert_eq!(
        code, 0,
        "brew --flavor failed:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let (code, stdout, stderr) = run_executable(&binary);
    assert_eq!(code, 0, "flavored binary failed:\n{stderr}");
    assert_eq!(stdout, "Hello, brew!\n");

    // The default build path (no flavor) keeps working and is still exposed
    // through both names.
    let plain = dir.path().join("hello-plain");
    let plain_arg = plain.to_str().expect("UTF-8 path");
    let (code, _stdout, stderr) = run_in(dir.path(), &["build", path, "-o", plain_arg]);
    assert_eq!(code, 0, "plain build failed:\n{stderr}");
    let (code, stdout, _stderr) = run_executable(&plain);
    assert_eq!(code, 0);
    assert_eq!(stdout, "Hello, brew!\n");

    let brewed = dir.path().join("hello-brew");
    let brewed_arg = brewed.to_str().expect("UTF-8 path");
    let (code, _stdout, stderr) = run_in(dir.path(), &["brew", path, "-o", brewed_arg]);
    assert_eq!(code, 0, "brew without a flavor failed:\n{stderr}");
    let (code, stdout, _stderr) = run_executable(&brewed);
    assert_eq!(code, 0);
    assert_eq!(stdout, "Hello, brew!\n");
}
