//! End-to-end tests for the NB-6 foreign-runtime bridges.
//!
//! Two bridges are covered through the `tea` CLI:
//!
//! - `_hydro_sidecar` (out-of-process): a deterministic CPython worker
//!   (`tests/fixtures/sidecar_worker.py`) is spawned with `python3 -u` and
//!   driven through the protocol-v1 frame stream. An independent CPython
//!   oracle, written by this test, speaks the same wire protocol directly and
//!   must agree with the Hydro program's results.
//! - `hna_adapter` (in-process): a real HNA v1 module compiled at test time
//!   from `tests/fixtures/hna_adapter.c` with `cc`, exercising `init`,
//!   `state()` and `shutdown()` under JIT and AOT, plus the loader's
//!   rejection of an initializer that returns a non-zero code.
//!
//! Skips are explicit: hosts without `python3` skip the sidecar tests, hosts
//! without `cc` skip the adapter tests, and the AOT half of each test is
//! skipped when `libhydro_runtime.a` is absent. Every test uses a fresh
//! temporary directory that is removed on drop; no test touches the network.

use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Hydro program driving the sidecar through every public operation.
const SIDECAR_SOURCE: &str = "\
import _hydro_sidecar as s

print(s.PROTOCOL_VERSION)

worker = s.spawn(\"python3\", [\"-u\", \"sidecar_worker.py\", \"--pid-file\", \"worker.pid\"])

print(s.call(worker, \"__hello__\", 1))
print(s.call(worker, \"echo\", 5))
print(s.call(worker, \"sum\", 2, 3))

try:
    s.call(worker, \"fail\")
except ValueError as error:
    print(\"ValueError \" + str(error))

print(s.alive(worker))
s.restart(worker)
print(s.alive(worker))
print(s.call(worker, \"echo\", 4 + 3))
s.close(worker)
print(s.alive(worker))

try:
    s.call(worker, \"echo\", 1)
except RuntimeError as error:
    print(\"RuntimeError \" + str(error))
";

const SIDECAR_EXPECTED: &str = "\
1
cpython-sidecar
5
5
ValueError worker failure
True
True
7
False
RuntimeError sidecar: handle is closed
";

/// Same worker, but the handshake answers with protocol version 2.
const VERSION_SOURCE: &str = "\
import _hydro_sidecar as s

try:
    s.spawn(\"python3\", [\"-u\", \"sidecar_worker.py\", \"--pid-file\", \"v2.pid\", \"--speak-version\", \"2\"])
except RuntimeError as error:
    print(\"RuntimeError \" + str(error))

worker = s.spawn(\"python3\", [\"-u\", \"sidecar_worker.py\", \"--pid-file\", \"worker.pid\"])
print(s.call(worker, \"echo\", 9))
s.close(worker)
";

const VERSION_EXPECTED: &str = "\
RuntimeError sidecar: worker speaks protocol version 2, expected 1
9
";

/// Independent CPython implementation of protocol v1 used as the oracle.
const ORACLE_SOURCE: &str = r#"#!/usr/bin/env python3
"""Independent CPython oracle for tea/tests/sidecar.rs (protocol v1)."""

import struct
import subprocess
import sys

VERSION = 1
KIND_REQUEST = 1
KIND_OK = 2
KIND_ERROR = 3
TAG_NONE = 0
TAG_FALSE = 1
TAG_TRUE = 2
TAG_INT = 3
TAG_STR = 5


def encode(value):
    if value is None:
        return bytes((TAG_NONE,))
    if value is True:
        return bytes((TAG_TRUE,))
    if value is False:
        return bytes((TAG_FALSE,))
    if isinstance(value, int):
        return bytes((TAG_INT,)) + struct.pack("<q", value)
    if isinstance(value, str):
        raw = value.encode("utf-8")
        return bytes((TAG_STR,)) + struct.pack("<I", len(raw)) + raw
    raise AssertionError("oracle cannot encode %r" % (value,))


def decode(body):
    tag = body[0]
    if tag == TAG_NONE:
        return None
    if tag == TAG_TRUE:
        return True
    if tag == TAG_FALSE:
        return False
    if tag == TAG_INT:
        return struct.unpack("<q", body[1:9])[0]
    if tag == TAG_STR:
        (size,) = struct.unpack("<I", body[1:5])
        assert len(body) == 5 + size, "trailing bytes in string"
        return body[5:].decode("utf-8")
    raise AssertionError("oracle cannot decode tag %d" % tag)


def request(ident, method, values, version=VERSION):
    body = struct.pack("<I", len(method)) + method.encode("utf-8")
    body += struct.pack("<I", len(values))
    for value in values:
        body += encode(value)
    payload = bytes((version,)) + struct.pack("<I", ident) + bytes((KIND_REQUEST,)) + body
    return struct.pack("<I", len(payload)) + payload


def read_exact(stream, count):
    data = b""
    while len(data) < count:
        chunk = stream.read(count - len(data))
        if not chunk:
            raise EOFError("worker closed its stdout")
        data += chunk
    return data


def read_response(stream):
    (length,) = struct.unpack("<I", read_exact(stream, 4))
    payload = read_exact(stream, length)
    version, ident, kind = payload[0], struct.unpack("<I", payload[1:5])[0], payload[5]
    body = payload[6:]
    if kind == KIND_OK:
        return version, ident, "ok", decode(body)
    assert kind == KIND_ERROR, "unexpected response kind %d" % kind
    tag = body[0]
    (size,) = struct.unpack("<I", body[1:5])
    return version, ident, "error", (tag, body[5:5 + size].decode("utf-8"))


class Worker:
    def __init__(self, extra=()):
        self.process = subprocess.Popen(
            [sys.executable, "-u", "sidecar_worker.py", *extra],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=sys.stderr,
        )
        self.next_id = 0

    def call(self, method, *values):
        ident = self.next_id
        self.next_id += 1
        self.process.stdin.write(request(ident, method, list(values)))
        self.process.stdin.flush()
        version, got, kind, payload = read_response(self.process.stdout)
        assert got == ident, "response id %d, expected %d" % (got, ident)
        return version, kind, payload

    def close(self):
        self.process.stdin.close()
        code = self.process.wait(timeout=5)
        assert code == 0, "worker exited with %d" % code


worker = Worker(["--pid-file", "oracle.pid"])
version, kind, name = worker.call("__hello__", 1)
assert (version, kind) == (VERSION, "ok"), (version, kind)
print(name)
version, kind, echoed = worker.call("echo", 5)
assert (version, kind, echoed) == (VERSION, "ok", 5)
print(echoed)
version, kind, total = worker.call("sum", 2, 3)
assert (version, kind, total) == (VERSION, "ok", 5)
print(total)
version, kind, failure = worker.call("fail")
assert (version, kind) == (VERSION, "error"), (version, kind)
tag, message = failure
assert tag == 0 and message == "worker failure", failure
print("ValueError " + message)
worker.close()

version_two = Worker(["--pid-file", "oracle-v2.pid", "--speak-version", "2"])
version, kind, name = version_two.call("__hello__", 1)
assert kind == "ok" and name == "cpython-sidecar", (kind, name)
assert version == 2, "worker answered version %r" % (version,)
print("version 2")
version_two.close()
"#;

const ORACLE_EXPECTED: &str = "\
cpython-sidecar
5
5
ValueError worker failure
version 2
";

/// In-process HNA adapter: `init` flips a static flag, `shutdown` clears it.
const ADAPTER_SOURCE: &str = "\
import hna_adapter

print(hna_adapter.state())
hna_adapter.shutdown()
print(hna_adapter.state())
";

const ADAPTER_EXPECTED: &str = "True\nFalse\n";

/// A descriptor whose `init` is NULL never sets the flag.
const ADAPTER_NO_INIT_SOURCE: &str = ADAPTER_SOURCE;
const ADAPTER_NO_INIT_EXPECTED: &str = "False\nFalse\n";

/// A descriptor whose `init` returns 7 without raising must be rejected.
const ADAPTER_FAIL_SOURCE: &str = "\
import hna_adapter

print(hna_adapter.state())
print(\"unreachable\")
";

struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new(test_name: &str) -> TestDir {
        let unique = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "tea-sidecar-{}-{test_name}-{unique}",
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

fn run_executable(path: &Path, dir: &Path) -> (i32, String, String) {
    let output = Command::new(path)
        .current_dir(dir)
        .output()
        .expect("failed to spawn built executable");
    output_parts(output)
}

fn python_available() -> bool {
    Command::new("python3")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn cc_available() -> bool {
    Command::new("cc")
        .arg("--version")
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

/// Copies the CPython worker fixture into the test directory.
fn install_worker(dir: &TestDir) {
    std::fs::copy(
        fixtures_dir().join("sidecar_worker.py"),
        dir.path().join("sidecar_worker.py"),
    )
    .expect("copy sidecar worker fixture");
}

/// Runs the independent CPython oracle and returns its stdout.
fn run_oracle(dir: &TestDir) -> String {
    dir.write("oracle.py", ORACLE_SOURCE);
    let output = Command::new("python3")
        .args(["-u", "oracle.py"])
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn python3 oracle");
    let (code, stdout, stderr) = output_parts(output);
    assert_eq!(code, 0, "CPython oracle failed:\n{stderr}");
    assert_eq!(
        stdout, ORACLE_EXPECTED,
        "unexpected oracle output:\n{stderr}"
    );
    stdout
}

/// Reads the worker pids appended by `--pid-file`.
fn recorded_pids(path: &Path) -> Vec<u32> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

/// Asserts every pid recorded in `path` is no longer a live worker.
#[cfg(target_os = "linux")]
fn assert_no_worker_remains(path: &Path) {
    let pids = recorded_pids(path);
    assert!(
        !pids.is_empty(),
        "the worker never recorded its pid in '{}'",
        path.display()
    );
    for pid in pids {
        let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        let text = String::from_utf8_lossy(&cmdline).replace('\0', " ");
        assert!(
            !text.contains("sidecar_worker.py"),
            "worker pid {pid} is still running after the program exited: {text:?}"
        );
    }
}

#[cfg(not(target_os = "linux"))]
fn assert_no_worker_remains(path: &Path) {
    let _ = path;
}

/// Compiles an HNA fixture as `<name>.hna.so` in `dir`.
fn compile_hna(dir: &TestDir, name: &str, source: &str, defines: &[&str]) -> PathBuf {
    let output = dir.path().join(format!("{name}.hna.so"));
    let source = fixtures_dir().join(source);
    let mut command = Command::new("cc");
    command
        .arg("-std=c99")
        .arg("-shared")
        .arg("-fPIC")
        .arg("-I")
        .arg(hydro_root().join("include"))
        .arg("-o")
        .arg(&output)
        .arg(&source);
    for define in defines {
        command.arg(format!("-D{define}"));
    }
    let result = command.output().expect("failed to spawn cc");
    assert!(
        result.status.success(),
        "failed to compile '{}' as '{name}.hna.so':\n{}",
        source.display(),
        String::from_utf8_lossy(&result.stderr)
    );
    output
}

/// Runs `main.py` under `tea build` and the resulting binary.
fn check_aot(dir: &TestDir, expected_stdout: &str) {
    if !runtime_lib_present() {
        eprintln!("skipping AOT check: libhydro_runtime.a is not present in the test profile");
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

/// The sidecar end-to-end case: handshake, echo, sum, remote ValueError,
/// alive, restart, close, and a closed-handle call, under JIT and AOT.
#[test]
fn cpython_sidecar_roundtrip_jit_and_aot() {
    if !python_available() {
        eprintln!("skipping sidecar test: `python3` is not available");
        return;
    }
    let dir = TestDir::new("roundtrip");
    install_worker(&dir);
    dir.write("main.py", SIDECAR_SOURCE);

    let (code, stdout, stderr) = run_tea(dir.path(), &["infuse", "main.py"]);
    assert_eq!(code, 0, "tea infuse failed:\n{stderr}");
    assert_eq!(
        stdout, SIDECAR_EXPECTED,
        "unexpected sidecar output:\n{stderr}"
    );
    assert_no_libpython(Path::new(env!("CARGO_BIN_EXE_tea")));
    assert_no_worker_remains(&dir.path().join("worker.pid"));

    // The CPython oracle performs the same operations through its own
    // protocol implementation; the first four result lines must agree.
    let oracle = run_oracle(&dir);
    let oracle_lines: Vec<&str> = oracle.lines().collect();
    let hydro_lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        &hydro_lines[1..5],
        &oracle_lines[..4],
        "sidecar results disagree with the CPython oracle:\n{stdout}\n--- oracle ---\n{oracle}"
    );

    check_aot(&dir, SIDECAR_EXPECTED);
    assert_no_worker_remains(&dir.path().join("worker.pid"));
}

/// The standalone CPython oracle also runs in its own test, so a broken
/// worker fixture is reported without depending on the Hydro binary.
#[test]
fn cpython_worker_oracle_is_deterministic() {
    if !python_available() {
        eprintln!("skipping sidecar oracle test: `python3` is not available");
        return;
    }
    let dir = TestDir::new("oracle");
    install_worker(&dir);
    let oracle = run_oracle(&dir);
    assert_eq!(oracle, ORACLE_EXPECTED);
    assert_no_worker_remains(&dir.path().join("oracle.pid"));
    assert_no_worker_remains(&dir.path().join("oracle-v2.pid"));
}

/// An unknown worker protocol version is rejected at spawn, the worker is
/// reaped, and the handle path still works afterwards.
#[test]
fn sidecar_rejects_unknown_worker_version() {
    if !python_available() {
        eprintln!("skipping sidecar version test: `python3` is not available");
        return;
    }
    let dir = TestDir::new("version");
    install_worker(&dir);
    dir.write("main.py", VERSION_SOURCE);

    let (code, stdout, stderr) = run_tea(dir.path(), &["infuse", "main.py"]);
    assert_eq!(code, 0, "tea infuse failed:\n{stderr}");
    assert_eq!(
        stdout, VERSION_EXPECTED,
        "unexpected version output:\n{stderr}"
    );
    assert_no_worker_remains(&dir.path().join("v2.pid"));
    assert_no_worker_remains(&dir.path().join("worker.pid"));

    check_aot(&dir, VERSION_EXPECTED);
    assert_no_worker_remains(&dir.path().join("v2.pid"));
    assert_no_worker_remains(&dir.path().join("worker.pid"));
}

/// The in-process HNA adapter: import runs `init`, `state()` observes it,
/// `shutdown()` clears it. Runs under JIT and AOT without libpython.
#[test]
fn hna_adapter_state_and_shutdown_jit_and_aot() {
    if !cc_available() {
        eprintln!("skipping HNA adapter test: `cc` is not available");
        return;
    }
    let dir = TestDir::new("adapter");
    let artifact = compile_hna(&dir, "hna_adapter", "hna_adapter.c", &[]);
    dir.write("main.py", ADAPTER_SOURCE);

    let (code, stdout, stderr) = run_tea(dir.path(), &["infuse", "main.py"]);
    assert_eq!(code, 0, "tea infuse failed:\n{stderr}");
    assert_eq!(
        stdout, ADAPTER_EXPECTED,
        "unexpected adapter output:\n{stderr}"
    );
    assert_no_libpython(Path::new(env!("CARGO_BIN_EXE_tea")));
    assert_no_libpython(&artifact);

    check_aot(&dir, ADAPTER_EXPECTED);
}

/// A descriptor with a NULL `init` is accepted; the flag is simply never set.
#[test]
fn hna_adapter_without_init_is_accepted() {
    if !cc_available() {
        eprintln!("skipping HNA adapter no-init test: `cc` is not available");
        return;
    }
    let dir = TestDir::new("adapter-no-init");
    compile_hna(
        &dir,
        "hna_adapter",
        "hna_adapter.c",
        &["HNA_ADAPTER_NO_INIT=1"],
    );
    dir.write("main.py", ADAPTER_NO_INIT_SOURCE);

    let (code, stdout, stderr) = run_tea(dir.path(), &["infuse", "main.py"]);
    assert_eq!(code, 0, "tea infuse failed:\n{stderr}");
    assert_eq!(
        stdout, ADAPTER_NO_INIT_EXPECTED,
        "unexpected no-init adapter output:\n{stderr}"
    );

    check_aot(&dir, ADAPTER_NO_INIT_EXPECTED);
}

/// A non-zero `init` return with no pending exception rejects the import.
#[test]
fn hna_adapter_failed_init_is_rejected() {
    if !cc_available() {
        eprintln!("skipping HNA adapter init-failure test: `cc` is not available");
        return;
    }
    let dir = TestDir::new("adapter-fail");
    compile_hna(
        &dir,
        "hna_adapter",
        "hna_adapter.c",
        &["HNA_ADAPTER_INIT_FAIL=1"],
    );
    dir.write("main.py", ADAPTER_FAIL_SOURCE);

    let (code, stdout, stderr) = run_tea(dir.path(), &["infuse", "main.py"]);
    assert_ne!(code, 0, "a failing init must reject the import:\n{stderr}");
    assert!(
        !stdout.contains("unreachable"),
        "the program continued after a failed init:\n{stdout}"
    );
    for needle in [
        "hna module 'hna_adapter'",
        "tea-hna-adapter-fixture 0.1.0",
        "init returned 7 and set no exception",
    ] {
        assert!(
            stderr.contains(needle),
            "missing diagnostic {needle:?} in:\n{stderr}"
        );
    }
}
