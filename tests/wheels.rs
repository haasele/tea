//! Integration tests for `tea install`.
//!
//! Wheels are built as synthetic ZIP archives in the test code: a pure-Python
//! wheel (with a deflated member), an abi3 extension wheel, and deliberately
//! broken archives for the rejection paths. The CLI is driven through
//! `CARGO_BIN_EXE_tea`, like the other CLI tests.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(test_name: &str) -> TempDir {
        let unique = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "tea-wheels-{}-{test_name}-{unique}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create temp dir");
        TempDir { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Runs `tea` in `dir` with a clean environment: inherited virtual
/// environments and site-package variables never leak into a test.
fn run_tea(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> (i32, String, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tea"));
    command
        .args(args)
        .current_dir(dir)
        .env_remove("VIRTUAL_ENV")
        .env_remove("CONDA_PREFIX")
        .env_remove("HYDRO_SITE_PACKAGES")
        .env_remove("HYDRO_EXTENSION_PATH")
        .env_remove("PYTHONPATH");
    for (key, value) in envs {
        command.env(key, value);
    }
    let output: Output = command.output().expect("failed to spawn tea binary");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn display(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

// --- synthetic wheel writer ------------------------------------------------

struct Member {
    name: String,
    data: Vec<u8>,
    /// Full Unix mode, including the file-type bits (0 = default).
    mode: u32,
    /// 0 = stored, 8 = deflate.
    method: u16,
}

impl Member {
    fn file(name: &str, data: &str) -> Member {
        Member {
            name: name.to_string(),
            data: data.as_bytes().to_vec(),
            mode: 0o100644,
            method: 0,
        }
    }

    fn deflated(name: &str, data: &str) -> Member {
        Member {
            name: name.to_string(),
            data: data.as_bytes().to_vec(),
            mode: 0o100644,
            method: 8,
        }
    }

    fn binary(name: &str, data: Vec<u8>, mode: u32) -> Member {
        Member {
            name: name.to_string(),
            data,
            mode,
            method: 0,
        }
    }

    fn directory(name: &str) -> Member {
        Member {
            name: name.to_string(),
            data: Vec::new(),
            mode: 0o040755,
            method: 0,
        }
    }

    fn symlink(name: &str, target: &str) -> Member {
        Member {
            name: name.to_string(),
            data: target.as_bytes().to_vec(),
            mode: 0o120777,
            method: 0,
        }
    }
}

fn build_wheel(members: &[Member]) -> Vec<u8> {
    let mut archive = Vec::new();
    let mut directory = Vec::new();

    for member in members {
        let offset = archive.len() as u32;
        let crc = crc32(&member.data);
        let compressed = match member.method {
            0 => member.data.clone(),
            8 => deflate_literals(&member.data),
            other => panic!("unsupported test compression method {other}"),
        };
        let name = member.name.as_bytes();
        let directory_entry = member.name.ends_with('/') || (member.mode & 0o170000) == 0o040000;

        write_u32(&mut archive, 0x0403_4b50);
        write_u16(&mut archive, 20);
        write_u16(&mut archive, 0);
        write_u16(&mut archive, member.method);
        write_u16(&mut archive, 0);
        write_u16(&mut archive, 0);
        write_u32(&mut archive, crc);
        write_u32(&mut archive, compressed.len() as u32);
        write_u32(&mut archive, member.data.len() as u32);
        write_u16(&mut archive, name.len() as u16);
        write_u16(&mut archive, 0);
        archive.extend_from_slice(name);
        archive.extend_from_slice(&compressed);

        write_u32(&mut directory, 0x0201_4b50);
        write_u16(&mut directory, 0x031E);
        write_u16(&mut directory, 20);
        write_u16(&mut directory, 0);
        write_u16(&mut directory, member.method);
        write_u16(&mut directory, 0);
        write_u16(&mut directory, 0);
        write_u32(&mut directory, crc);
        write_u32(&mut directory, compressed.len() as u32);
        write_u32(&mut directory, member.data.len() as u32);
        write_u16(&mut directory, name.len() as u16);
        write_u16(&mut directory, 0);
        write_u16(&mut directory, 0);
        write_u16(&mut directory, 0);
        write_u16(&mut directory, 0);
        write_u32(
            &mut directory,
            (member.mode << 16) | if directory_entry { 0x10 } else { 0 },
        );
        write_u32(&mut directory, offset);
        directory.extend_from_slice(name);
    }

    let directory_offset = archive.len() as u32;
    let directory_size = directory.len() as u32;
    archive.extend_from_slice(&directory);
    write_u32(&mut archive, 0x0605_4b50);
    write_u16(&mut archive, 0);
    write_u16(&mut archive, 0);
    write_u16(&mut archive, members.len() as u16);
    write_u16(&mut archive, members.len() as u16);
    write_u32(&mut archive, directory_size);
    write_u32(&mut archive, directory_offset);
    write_u16(&mut archive, 0);
    archive
}

fn write_u16(buffer: &mut Vec<u8>, value: u16) {
    buffer.extend_from_slice(&value.to_le_bytes());
}

fn write_u32(buffer: &mut Vec<u8>, value: u32) {
    buffer.extend_from_slice(&value.to_le_bytes());
}

/// A fixed-Huffman deflate stream that emits every byte as a literal.
fn deflate_literals(data: &[u8]) -> Vec<u8> {
    let mut writer = BitWriter::new();
    writer.bits(1, 1); // BFINAL
    writer.bits(1, 2); // BTYPE = 01 (fixed Huffman)
    for &byte in data {
        write_fixed_symbol(&mut writer, u16::from(byte));
    }
    write_fixed_symbol(&mut writer, 256); // end of block
    writer.finish()
}

fn write_fixed_symbol(writer: &mut BitWriter, symbol: u16) {
    let (code, length) = match symbol {
        0..=143 => (0x30 + u32::from(symbol), 8),
        144..=255 => (0x190 + u32::from(symbol - 144), 9),
        256..=279 => (u32::from(symbol - 256), 7),
        280..=287 => (0xC0 + u32::from(symbol - 280), 8),
        other => panic!("no fixed Huffman code for symbol {other}"),
    };
    for shift in (0..length).rev() {
        writer.bit(((code >> shift) & 1) as u8);
    }
}

struct BitWriter {
    bytes: Vec<u8>,
    current: u8,
    used: u32,
}

impl BitWriter {
    fn new() -> BitWriter {
        BitWriter {
            bytes: Vec::new(),
            current: 0,
            used: 0,
        }
    }

    fn bit(&mut self, value: u8) {
        if value != 0 {
            self.current |= 1 << self.used;
        }
        self.used += 1;
        if self.used == 8 {
            self.bytes.push(self.current);
            self.current = 0;
            self.used = 0;
        }
    }

    fn bits(&mut self, value: u32, count: u32) {
        for index in 0..count {
            self.bit(((value >> index) & 1) as u8);
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.used != 0 {
            self.bytes.push(self.current);
        }
        self.bytes
    }
}

fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (index, slot) in table.iter_mut().enumerate() {
        let mut value = index as u32;
        for _ in 0..8 {
            value = if value & 1 != 0 {
                0xEDB8_8320 ^ (value >> 1)
            } else {
                value >> 1
            };
        }
        *slot = value;
    }
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        let index = ((crc ^ u32::from(byte)) & 0xFF) as usize;
        crc = table[index] ^ (crc >> 8);
    }
    !crc
}

// --- fixtures --------------------------------------------------------------

const PURE_WHEEL: &str = "pkg-1.0-py3-none-any.whl";

const PURE_MEMBERS: &[(&str, &str)] = &[
    ("pkg/__init__.py", "VALUE = 42\n"),
    ("pkg/mod.py", "def add(a, b):\n    return a + b\n"),
];

fn pure_wheel_members() -> Vec<Member> {
    vec![
        Member::directory("pkg/"),
        Member::deflated("pkg/__init__.py", PURE_MEMBERS[0].1),
        Member::file("pkg/mod.py", PURE_MEMBERS[1].1),
        Member::directory("pkg/sub/"),
        Member::file("pkg/sub/value.txt", "stored verbatim\n"),
        Member::directory("pkg-1.0.dist-info/"),
        Member::file(
            "pkg-1.0.dist-info/METADATA",
            "Metadata-Version: 2.1\nName: pkg\nVersion: 1.0\n",
        ),
        Member::file(
            "pkg-1.0.dist-info/WHEEL",
            "Wheel-Version: 1.0\nGenerator: tea-wheels-test\nRoot-Is-Purelib: true\nTag: py3-none-any\n",
        ),
        Member::file("pkg-1.0.dist-info/RECORD", "pkg/__init__.py,,\n"),
    ]
}

fn write_wheel(dir: &Path, file_name: &str, members: &[Member]) -> PathBuf {
    let path = dir.join(file_name);
    fs::write(&path, build_wheel(members)).expect("write synthetic wheel");
    path
}

fn file_member_count(members: &[Member]) -> usize {
    members
        .iter()
        .filter(|member| !member.name.ends_with('/') && (member.mode & 0o170000) != 0o040000)
        .count()
}

// --- tests -----------------------------------------------------------------

#[test]
fn installs_pure_python_wheel_and_preserves_contents() {
    let temp = TempDir::new("pure");
    let members = pure_wheel_members();
    let wheel = write_wheel(temp.path(), PURE_WHEEL, &members);
    let target = temp.path().join("site");

    let (code, stdout, stderr) = run_tea(
        temp.path(),
        &[
            "install",
            &display(&wheel),
            "--target",
            &display(&target),
            "--verbose",
        ],
        &[],
    );
    assert_eq!(code, 0, "install failed:\n{stderr}");
    assert!(
        stdout.contains(display(&target).as_str()),
        "verbose output must name the target:\n{stdout}"
    );
    assert!(
        stdout.contains(format!("{} files", file_member_count(&members)).as_str()),
        "verbose output must report the installed file count:\n{stdout}"
    );

    assert_eq!(
        fs::read_to_string(target.join("pkg/__init__.py")).expect("installed __init__.py"),
        PURE_MEMBERS[0].1
    );
    assert_eq!(
        fs::read_to_string(target.join("pkg/mod.py")).expect("installed mod.py"),
        PURE_MEMBERS[1].1
    );
    assert_eq!(
        fs::read_to_string(target.join("pkg/sub/value.txt")).expect("installed value.txt"),
        "stored verbatim\n"
    );
    assert!(target.join("pkg-1.0.dist-info/METADATA").is_file());
    assert!(target.join("pkg-1.0.dist-info/WHEEL").is_file());
    assert!(target.join("pkg-1.0.dist-info/RECORD").is_file());
    assert_eq!(
        fs::read_to_string(target.join("pkg-1.0.dist-info/INSTALLER"))
            .expect("installed INSTALLER record"),
        "tea\n"
    );

    // The staging directory must not survive a successful install.
    for entry in fs::read_dir(temp.path()).expect("list temp dir") {
        let name = entry.expect("temp entry").file_name();
        assert!(
            !name.to_string_lossy().starts_with(".tea-install-"),
            "staging directory left behind: {}",
            name.to_string_lossy()
        );
    }
}

#[test]
fn install_defaults_to_the_project_local_site_packages() {
    let temp = TempDir::new("local-default");
    let wheel = write_wheel(temp.path(), PURE_WHEEL, &pure_wheel_members());

    let (code, _stdout, stderr) = run_tea(temp.path(), &["install", &display(&wheel)], &[]);
    assert_eq!(code, 0, "install failed:\n{stderr}");
    assert!(temp
        .path()
        .join(".tea/site-packages/pkg/__init__.py")
        .is_file());
}

#[test]
fn install_defaults_to_virtual_env_site_packages() {
    let temp = TempDir::new("venv-default");
    let wheel = write_wheel(temp.path(), PURE_WHEEL, &pure_wheel_members());
    let venv = temp.path().join("venv");
    fs::create_dir_all(&venv).expect("create virtualenv");

    let (code, _stdout, stderr) = run_tea(
        temp.path(),
        &["install", &display(&wheel)],
        &[("VIRTUAL_ENV", &display(&venv))],
    );
    assert_eq!(code, 0, "install failed:\n{stderr}");
    assert!(venv
        .join("lib/python3.14/site-packages/pkg/__init__.py")
        .is_file());
}

#[test]
fn rejects_incompatible_wheel_tags_without_writing() {
    let temp = TempDir::new("tags");
    let members = pure_wheel_members();
    let wheels = [
        ("pkg-1.0-cp312-cp312-manylinux_2_17_x86_64.whl", "ABI"),
        ("pkg-1.0-py2-none-any.whl", "Python"),
        ("pkg-1.0-py3-none-plan9_x86_64.whl", "platform"),
    ];

    for (index, (name, _kind)) in wheels.iter().enumerate() {
        let wheel = write_wheel(temp.path(), name, &members);
        let target = temp.path().join(format!("site-{index}"));
        let (code, stdout, stderr) = run_tea(
            temp.path(),
            &["install", &display(&wheel), "--target", &display(&target)],
            &[],
        );
        assert_eq!(code, 1, "{name} must be rejected:\n{stderr}");
        assert!(
            stderr.contains("incompatible wheel"),
            "{name}: missing incompatible tag error:\n{stderr}"
        );
        assert!(
            !stdout.contains("installed"),
            "{name}: must not report success"
        );
        assert!(!target.exists(), "{name}: target must not be created");
    }
}

#[test]
fn rejects_unsafe_member_paths_without_writing() {
    let temp = TempDir::new("traversal");
    let absolute =
        std::env::temp_dir().join(format!("tea-wheels-absolute-escape-{}", std::process::id()));
    let _ = fs::remove_file(&absolute);
    let absolute_name = display(&absolute);

    let members = [
        vec![Member::file("../escaped.py", "escaped\n")],
        vec![Member::file("pkg/../../escaped.py", "escaped\n")],
        vec![Member::file(&absolute_name, "escaped\n")],
    ];

    for (index, members) in members.iter().enumerate() {
        let wheel = write_wheel(
            temp.path(),
            &format!("evil-{index}-1.0-py3-none-any.whl"),
            members,
        );
        let target = temp.path().join(format!("site-{index}"));
        let (code, _stdout, stderr) = run_tea(
            temp.path(),
            &["install", &display(&wheel), "--target", &display(&target)],
            &[],
        );
        assert_eq!(code, 1, "unsafe member must be rejected:\n{stderr}");
        assert!(
            stderr.contains("invalid wheel"),
            "missing invalid-wheel error:\n{stderr}"
        );
        assert!(!target.exists(), "target must not be created");
        assert!(!temp.path().join("escaped.py").exists());
        assert!(!absolute.exists(), "absolute member must not be written");
    }
}

#[test]
fn rejects_symlink_and_duplicate_members() {
    let temp = TempDir::new("member-kinds");

    let symlink_wheel = write_wheel(
        temp.path(),
        "evil-symlink-1.0-py3-none-any.whl",
        &[
            Member::file("pkg/__init__.py", ""),
            Member::symlink("pkg/passwd", "/etc/passwd"),
        ],
    );
    let target = temp.path().join("site-symlink");
    let (code, _stdout, stderr) = run_tea(
        temp.path(),
        &[
            "install",
            &display(&symlink_wheel),
            "--target",
            &display(&target),
        ],
        &[],
    );
    assert_eq!(code, 1, "symlink member must be rejected:\n{stderr}");
    assert!(
        stderr.contains("symlink"),
        "missing symlink error:\n{stderr}"
    );
    assert!(!target.exists());

    let duplicate_wheel = write_wheel(
        temp.path(),
        "evil-duplicate-1.0-py3-none-any.whl",
        &[
            Member::file("pkg/__init__.py", "first\n"),
            Member::file("pkg/__init__.py", "second\n"),
        ],
    );
    let target = temp.path().join("site-duplicate");
    let (code, _stdout, stderr) = run_tea(
        temp.path(),
        &[
            "install",
            &display(&duplicate_wheel),
            "--target",
            &display(&target),
        ],
        &[],
    );
    assert_eq!(code, 1, "duplicate member must be rejected:\n{stderr}");
    assert!(
        stderr.contains("duplicate"),
        "missing duplicate member error:\n{stderr}"
    );
    assert!(!target.exists());
}

#[test]
fn truncated_wheel_is_rejected_without_claiming_success() {
    let temp = TempDir::new("truncated");
    let bytes = build_wheel(&pure_wheel_members());
    let wheel = temp.path().join(PURE_WHEEL);
    fs::write(&wheel, &bytes[..bytes.len() / 2]).expect("write truncated wheel");
    let target = temp.path().join("site");

    let (code, stdout, stderr) = run_tea(
        temp.path(),
        &["install", &display(&wheel), "--target", &display(&target)],
        &[],
    );
    assert_eq!(code, 1, "truncated wheel must fail:\n{stderr}");
    assert!(!stdout.contains("installed"), "must not claim success");
    assert!(!target.exists(), "target must not be created");
}

#[test]
fn failed_extraction_leaves_no_target_or_staging_directory() {
    let temp = TempDir::new("extract-failure");
    let members = vec![Member::file("pkg/data.txt", "original\n")];
    let mut bytes = build_wheel(&members);
    // The first member is stored directly after its 30-byte local header and
    // 12-byte name; corrupting the payload keeps the ZIP structure valid but
    // makes the CRC check fail during extraction.
    let data_offset = 30 + "pkg/data.txt".len();
    bytes[data_offset] ^= 0xFF;
    let wheel = temp.path().join("pkg-1.0-py3-none-any.whl");
    fs::write(&wheel, bytes).expect("write corrupt wheel");
    let target = temp.path().join("site");

    let (code, stdout, stderr) = run_tea(
        temp.path(),
        &["install", &display(&wheel), "--target", &display(&target)],
        &[],
    );
    assert_eq!(code, 1, "corrupt payload must fail:\n{stderr}");
    assert!(!stdout.contains("installed"), "must not claim success");
    assert!(!target.exists(), "target must not be created");
    for entry in fs::read_dir(temp.path()).expect("list temp dir") {
        let name = entry.expect("temp entry").file_name();
        assert!(
            !name.to_string_lossy().starts_with(".tea-install-"),
            "staging directory left behind: {}",
            name.to_string_lossy()
        );
    }
}

#[test]
fn install_usage_errors_exit_with_status_2() {
    let temp = TempDir::new("usage");
    let (code, _stdout, stderr) = run_tea(temp.path(), &["install"], &[]);
    assert_eq!(code, 2, "missing wheel is a usage error:\n{stderr}");
    assert!(stderr.contains("missing wheel file"));

    let (code, _stdout, stderr) = run_tea(temp.path(), &["install", PURE_WHEEL, "--bogus"], &[]);
    assert_eq!(code, 2, "unknown option is a usage error:\n{stderr}");
    assert!(stderr.contains("unsupported option '--bogus'"));
}

#[test]
fn help_lists_the_install_command() {
    let temp = TempDir::new("help");
    let (code, stdout, stderr) = run_tea(temp.path(), &["--help"], &[]);
    assert_eq!(code, 0, "help failed:\n{stderr}");
    assert!(
        stdout.contains("tea install <wheel.whl>"),
        "--help must list tea install:\n{stdout}"
    );
    assert!(
        stdout.contains("--target DIR"),
        "--help must document --target:\n{stdout}"
    );
}

#[test]
fn installed_pure_wheel_is_importable_through_hydro_site_packages() {
    let temp = TempDir::new("infuse");
    let wheel = write_wheel(temp.path(), PURE_WHEEL, &pure_wheel_members());
    let target = temp.path().join("site");
    let (code, _stdout, stderr) = run_tea(
        temp.path(),
        &["install", &display(&wheel), "--target", &display(&target)],
        &[],
    );
    assert_eq!(code, 0, "install failed:\n{stderr}");

    let program = temp.path().join("app.py");
    fs::write(
        &program,
        "import pkg\nprint(pkg.VALUE)\nfrom pkg.mod import add\nprint(add(2, 3))\n",
    )
    .expect("write app.py");
    let (code, stdout, stderr) = run_tea(
        temp.path(),
        &["infuse", &display(&program)],
        &[("HYDRO_SITE_PACKAGES", &display(&target))],
    );
    assert_eq!(code, 0, "tea infuse failed:\n{stderr}");
    assert_eq!(stdout, "42\n5\n");
}

#[test]
fn installs_abi3_extension_wheel_and_imports_it() {
    let Some(toolchain) = detect_toolchain() else {
        return;
    };
    let Some(platform) = native_platform_tag() else {
        eprintln!("skipping abi3 install test: unsupported host platform");
        return;
    };

    let temp = TempDir::new("abi3");
    let build = temp.path().join("build");
    fs::create_dir_all(&build).expect("create build dir");
    let module = build.join("hydro_capi_probe.abi3.so");
    toolchain.build_probe(&build, &module);

    let wheel_name = format!("hydro_capi_probe-1.0-cp314-abi3-{platform}.whl");
    let members = vec![
        Member::binary(
            "hydro_capi_probe.abi3.so",
            fs::read(&module).expect("read compiled probe"),
            0o100755,
        ),
        Member::directory("hydro_capi_probe-1.0.dist-info/"),
        Member::file(
            "hydro_capi_probe-1.0.dist-info/METADATA",
            "Metadata-Version: 2.1\nName: hydro-capi-probe\nVersion: 1.0\n",
        ),
        Member::file(
            "hydro_capi_probe-1.0.dist-info/WHEEL",
            &format!(
                "Wheel-Version: 1.0\nGenerator: tea-wheels-test\nRoot-Is-Purelib: false\nTag: cp314-abi3-{platform}\n"
            ),
        ),
    ];
    let wheel = write_wheel(temp.path(), &wheel_name, &members);
    let target = temp.path().join("site");
    let (code, _stdout, stderr) = run_tea(
        temp.path(),
        &["install", &display(&wheel), "--target", &display(&target)],
        &[],
    );
    assert_eq!(code, 0, "abi3 wheel install failed:\n{stderr}");

    let installed = target.join("hydro_capi_probe.abi3.so");
    assert!(installed.is_file(), "extension was not installed");
    assert_eq!(
        fs::read(&installed).expect("read installed extension"),
        fs::read(&module).expect("read compiled probe")
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&installed)
            .expect("extension metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111, "executable bits must be preserved");
    }

    let program = temp.path().join("app.py");
    fs::write(
        &program,
        "import hydro_capi_probe as p\nprint(p.add(2, 3))\nprint(p.tag)\n",
    )
    .expect("write app.py");
    let (code, stdout, stderr) = run_tea(
        temp.path(),
        &["infuse", &display(&program)],
        &[("HYDRO_SITE_PACKAGES", &display(&target))],
    );
    assert_eq!(
        code, 0,
        "importing the installed extension failed:\n{stderr}"
    );
    assert_eq!(stdout, "5\nabi3\n");
}

// --- host toolchain and platform helpers -----------------------------------

struct Toolchain {
    includes: Vec<String>,
}

impl Toolchain {
    fn build_probe(&self, dir: &Path, output: &Path) {
        let source = dir.join("abi3_probe.c");
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/abi3_probe.c");
        fs::copy(&fixture, &source).expect("copy abi3 fixture");
        let mut command = Command::new("cc");
        command.arg("-shared").arg("-fPIC");
        for include in &self.includes {
            command.arg(include);
        }
        command.arg("-o").arg(output).arg(&source);
        let result = command.output().expect("failed to spawn cc");
        assert!(
            result.status.success(),
            "failed to compile the abi3 probe:\n{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

fn detect_toolchain() -> Option<Toolchain> {
    match Command::new("cc").arg("--version").output() {
        Ok(output) if output.status.success() => {}
        _ => {
            eprintln!("skipping abi3 install test: `cc` is not available");
            return None;
        }
    }
    let output = match Command::new("python3-config").arg("--includes").output() {
        Ok(output) if output.status.success() => output,
        _ => {
            eprintln!("skipping abi3 install test: `python3-config` is not available");
            return None;
        }
    };
    let includes: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .filter(|token| token.starts_with("-I"))
        .map(str::to_owned)
        .collect();
    let headers_found = includes.iter().any(|include| {
        Path::new(include.trim_start_matches("-I"))
            .join("Python.h")
            .is_file()
    });
    if !headers_found {
        eprintln!("skipping abi3 install test: Python.h is not installed");
        return None;
    }
    Some(Toolchain { includes })
}

fn native_platform_tag() -> Option<String> {
    Some(match std::env::consts::OS {
        "linux" => format!("linux_{}", wheel_arch()),
        "macos" => format!("macosx_11_0_{}", wheel_arch()),
        "windows" => match std::env::consts::ARCH {
            "x86_64" => "win_amd64".to_string(),
            "x86" => "win32".to_string(),
            "aarch64" => "win_arm64".to_string(),
            _ => return None,
        },
        _ => return None,
    })
}

fn wheel_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86" => "i686",
        "arm" => "armv7l",
        "powerpc64" => {
            if cfg!(target_endian = "little") {
                "ppc64le"
            } else {
                "ppc64"
            }
        }
        "aarch64" if std::env::consts::OS == "macos" => "arm64",
        other => other,
    }
}
