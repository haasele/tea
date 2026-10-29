//! `tea install <wheel.whl>`: validate and unpack a wheel into site-packages.
//!
//! A wheel is an ordinary ZIP archive. The installer reads the archive into
//! memory, validates the filename tags against the CPython target advertised by
//! [`hydro_ir`] (or, for a Hydro-native wheel, against the supported
//! `hydro1_0` ABI on Linux x86_64), and validates every member name, type and
//! compression method *before* it writes anything. The archive is then unpacked
//! into a temporary directory next to the target; a new target is created with
//! a single atomic rename, and an existing target is updated with per-file
//! renames after the whole archive has been read. A rejected or truncated wheel
//! therefore never reports success or leaves a half-extracted package behind.
//!
//! Members are written verbatim: package files are never rewritten, and the
//! wheel's `.dist-info` directory is preserved. The only added file is
//! `.dist-info/INSTALLER` (when the wheel does not already ship one).
//!
//! ZIP support is deliberately small: end-of-central-directory (including the
//! ZIP64 record), central-directory parsing, and the `stored` and `deflate`
//! compression methods used by wheels. The deflate decoder follows RFC 1951 and
//! is limited by the size declared in the central directory.

use std::collections::HashSet;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use hydro_ir::{PYTHON_ABI_TAG, PYTHON_VERSION, PYTHON_VERSION_STRING};

use crate::cli::args::{parse_install, Verbosity};
use crate::cli::{CliError, CliResult};

/// Default target below the project root when no virtual environment is active.
const LOCAL_SITE_PACKAGES: &str = ".tea/site-packages";

/// Contents of the installation record written into `.dist-info/INSTALLER`.
const INSTALLER_CONTENT: &str = "tea\n";

/// ABI tag of a Hydro-native wheel (`py3-none-hydro1_0_<platform>`), see
/// `docs/NATIVE_BINDINGS.md` §4.3 in the Hydro repository. Installation only
/// recognizes the tag; it does not claim that the runtime can load HNA
/// artifacts.
const HYDRO_ABI_TAG: &str = "hydro1_0";

/// Python tag required by a Hydro-native wheel.
const HYDRO_PYTHON_TAG: &str = "py3";

/// Prefix of the temporary directory created next to the target.
const STAGING_PREFIX: &str = ".tea-install-";

static STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) fn cmd_install(args: &[OsString], verbosity: Verbosity) -> CliResult {
    let options = parse_install(args, verbosity)?;
    let wheel = WheelName::parse(&options.wheel)?;
    wheel.check_compatible()?;

    let target = target_for(&options);
    let archive = ZipArchive::open(&options.wheel)?;
    let summary = install_archive(&archive, &target)?;

    if !options.verbosity.is_quiet() {
        println!(
            "installed {} {} into '{}'",
            wheel.distribution,
            wheel.version,
            target.display()
        );
        if options.verbosity.is_verbose() {
            println!("tea install: wheel '{}'", options.wheel.display());
            println!("tea install: target '{}'", target.display());
            println!("tea install: installed {} files", summary.files);
            println!(
                "tea install: loadable extension suffixes: <leaf>.so, <leaf>.abi3.so, \
                 <leaf>.{PYTHON_ABI_TAG}-<platform>.so"
            );
            println!(
                "tea install: hydro native artifacts: <leaf>.hna.so (wheel ABI tag \
                 '{HYDRO_ABI_TAG}'; runtime loading is not implemented yet)"
            );
        }
        if !target_is_discovered(&target) {
            println!(
                "note: set HYDRO_SITE_PACKAGES='{}' so `tea infuse` can import installed modules",
                target.display()
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Resolve the site-packages directory: `--target`, then `VIRTUAL_ENV`, then
/// the project-local `.tea/site-packages`.
fn target_for(options: &crate::cli::args::InstallOptions) -> PathBuf {
    if let Some(target) = &options.target {
        return target.clone();
    }
    if let Some(venv) = env::var_os("VIRTUAL_ENV").filter(|value| !value.is_empty()) {
        let (major, minor, _) = PYTHON_VERSION;
        return Path::new(&venv)
            .join("lib")
            .join(format!("python{major}.{minor}"))
            .join("site-packages");
    }
    PathBuf::from(LOCAL_SITE_PACKAGES)
}

/// Whether the frontend already searches `target` without extra configuration.
fn target_is_discovered(target: &Path) -> bool {
    let (major, minor, _) = PYTHON_VERSION;
    let mut candidates = Vec::new();
    for variable in ["VIRTUAL_ENV", "CONDA_PREFIX"] {
        if let Some(prefix) = env::var_os(variable).filter(|value| !value.is_empty()) {
            candidates.push(
                Path::new(&prefix)
                    .join("lib")
                    .join(format!("python{major}.{minor}"))
                    .join("site-packages"),
            );
        }
    }
    for variable in ["HYDRO_SITE_PACKAGES", "PYTHONPATH"] {
        if let Some(value) = env::var_os(variable) {
            candidates.extend(env::split_paths(&value));
        }
    }
    candidates
        .iter()
        .any(|candidate| same_path(candidate, target))
}

fn same_path(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

// --- wheel filename tags ---------------------------------------------------

struct WheelName {
    distribution: String,
    version: String,
    /// The optional wheel build field. The documented Hydro spelling
    /// `py3-none-hydro1_0-<platform>` puts `py3` in this slot.
    build_tag: Option<String>,
    python_tags: Vec<String>,
    abi_tags: Vec<String>,
    platform_tags: Vec<String>,
}

impl WheelName {
    fn parse(path: &Path) -> Result<WheelName, CliError> {
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| install_error(format!("'{}' has no wheel filename", path.display())))?;
        let stem = file_name.strip_suffix(".whl").ok_or_else(|| {
            install_error(format!(
                "'{file_name}' is not a .whl file (expected <name>-<version>-<python>-<abi>-<platform>.whl)"
            ))
        })?;
        let parts: Vec<&str> = stem.split('-').collect();
        let (distribution, version, build, python, abi, platform) = match parts.as_slice() {
            [distribution, version, python, abi, platform] => {
                (*distribution, *version, None, *python, *abi, *platform)
            }
            [distribution, version, build, python, abi, platform] => {
                if build.is_empty() {
                    return Err(install_error(format!(
                        "'{file_name}' has an empty wheel build tag"
                    )));
                }
                (
                    *distribution,
                    *version,
                    Some(*build),
                    *python,
                    *abi,
                    *platform,
                )
            }
            _ => {
                return Err(install_error(format!(
                    "'{file_name}' is not a wheel filename (expected \
                     <name>-<version>-<python>-<abi>-<platform>.whl)"
                )))
            }
        };
        if distribution.is_empty() || version.is_empty() {
            return Err(install_error(format!(
                "'{file_name}' has an empty distribution or version"
            )));
        }
        let python_tags = split_tags(python);
        let abi_tags = split_tags(abi);
        let platform_tags = split_tags(platform);
        if python_tags.is_empty() || abi_tags.is_empty() || platform_tags.is_empty() {
            return Err(install_error(format!(
                "'{file_name}' has an empty wheel compatibility tag"
            )));
        }
        Ok(WheelName {
            distribution: distribution.to_string(),
            version: version.to_string(),
            build_tag: build.map(str::to_string),
            python_tags,
            abi_tags,
            platform_tags,
        })
    }

    fn check_compatible(&self) -> Result<(), CliError> {
        // A Hydro ABI tag routes through its own rules: the CPython ABI
        // ladder (`none`/`abi3`/`cp3NN`) does not describe HNA artifacts.
        // The documented spelling carries the ABI either in the ABI field
        // (`py3-hydro1_0-<platform>`) or as a platform-tag prefix
        // (`py3-none-hydro1_0_<platform>`).
        if self.hydro_abi_requested() {
            return self.check_hydro_compatible();
        }
        for abi in &self.abi_tags {
            if !self.platforms_compatible(abi) {
                continue;
            }
            for python in &self.python_tags {
                if pair_compatible(python, abi) {
                    return Ok(());
                }
            }
        }
        Err(install_error(format!(
            "incompatible wheel: {}-{}-{} is not usable with CPython {PYTHON_VERSION_STRING} \
             on {}-{} (Python, ABI or platform tags do not match)",
            self.python_tags.join("."),
            self.abi_tags.join("."),
            self.platform_tags.join("."),
            env::consts::ARCH,
            env::consts::OS
        )))
    }

    /// Whether any tag names a Hydro ABI, so the hydro diagnostics apply
    /// instead of the CPython ones.
    fn hydro_abi_requested(&self) -> bool {
        self.abi_tags
            .iter()
            .any(|abi| split_hydro_abi(abi).is_some())
            || self
                .platform_tags
                .iter()
                .any(|tag| split_hydro_abi(tag).is_some())
    }

    /// Hydro-native wheels are accepted only for the documented tag: Python
    /// `py3`, ABI [`HYDRO_ABI_TAG`] and a Linux x86_64 platform. The platform
    /// is never `any`: HNA artifacts are native binaries.
    fn check_hydro_compatible(&self) -> Result<(), CliError> {
        let abi_ok = self.abi_tags.iter().any(|abi| abi == HYDRO_ABI_TAG)
            || self
                .platform_tags
                .iter()
                .any(|tag| split_hydro_abi(tag).is_some_and(|(abi, _)| abi == HYDRO_ABI_TAG));
        // `py3-none-hydro1_0-<platform>` is the documented spelling; the wheel
        // grammar reads it as build `py3`, Python tag `none`, ABI `hydro1_0`,
        // so the build field carries the Python tag there.
        let python_ok = self.python_tags.iter().any(|tag| tag == HYDRO_PYTHON_TAG)
            || (self.python_tags.iter().any(|tag| tag == "none")
                && self.build_tag.as_deref() == Some(HYDRO_PYTHON_TAG));
        let platform_ok = self
            .platform_tags
            .iter()
            .any(|tag| hydro_platform_tag_matches(tag));
        if abi_ok && python_ok && platform_ok {
            return Ok(());
        }
        Err(install_error(format!(
            "incompatible wheel: {}-{}-{} is not a supported Hydro native wheel \
             (expected Python tag '{HYDRO_PYTHON_TAG}', ABI tag '{HYDRO_ABI_TAG}' and a \
             linux_x86_64 or manylinux*_x86_64 platform; supported Hydro ABI: {HYDRO_ABI_TAG})",
            self.python_tags.join("."),
            self.abi_tags.join("."),
            self.platform_tags.join(".")
        )))
    }

    fn platforms_compatible(&self, abi: &str) -> bool {
        let pure = abi == "none";
        self.platform_tags.iter().any(|tag| {
            if tag == "any" {
                pure
            } else {
                native_platform_matches(tag)
            }
        })
    }
}

fn split_tags(tags: &str) -> Vec<String> {
    tags.split('.')
        .filter(|tag| !tag.is_empty())
        .map(str::to_string)
        .collect()
}

/// Split a Hydro ABI tag (`hydro<major>_<minor>`) from the front of `tag`.
/// Returns the ABI token and the remainder after the separating `_`, if any:
/// `hydro1_0` → (`hydro1_0`, `None`), `hydro1_0_linux_x86_64` →
/// (`hydro1_0`, `Some("linux_x86_64")`). Anything else is `None`, so
/// CPython tags never take the Hydro path.
fn split_hydro_abi(tag: &str) -> Option<(&str, Option<&str>)> {
    let rest = tag.strip_prefix("hydro")?;
    let (major, rest) = rest.split_once('_')?;
    if major.is_empty() || !major.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let (minor, tail) = match rest.split_once('_') {
        Some((minor, tail)) => (minor, Some(tail)),
        None => (rest, None),
    };
    if minor.is_empty() || !minor.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let token_len = "hydro".len() + major.len() + 1 + minor.len();
    Some((&tag[..token_len], tail))
}

/// The concrete ABI tag (`cp314`) corresponding to [`PYTHON_ABI_TAG`].
fn concrete_abi_tag() -> String {
    match PYTHON_ABI_TAG.strip_prefix("cpython-") {
        Some(digits) => format!("cp{digits}"),
        None => {
            let (major, minor, _) = PYTHON_VERSION;
            format!("cp{major}{minor}")
        }
    }
}

fn pair_compatible(python: &str, abi: &str) -> bool {
    let (major, minor, _) = PYTHON_VERSION;
    match abi {
        "none" => pure_python_tag(python, major, minor),
        "abi3" => abi3_python_tag(python, major, minor),
        tag if tag == concrete_abi_tag().as_str() => python == tag,
        _ => false,
    }
}

/// A pure-Python wheel runs on the target when its tag is `py3` or names a
/// Python 3 minor release no newer than the target.
fn pure_python_tag(tag: &str, major: u32, minor: u32) -> bool {
    if tag == "py3" {
        return major == 3;
    }
    match parse_python_tag(tag) {
        Some((_, tag_major, tag_minor)) => tag_major == major && tag_minor <= minor,
        None => false,
    }
}

/// The stable ABI is backward compatible: a `cp3N-abi3` wheel runs on any
/// CPython 3 minor release at least `N`.
fn abi3_python_tag(tag: &str, major: u32, minor: u32) -> bool {
    match parse_python_tag(tag) {
        Some(("cp", tag_major, tag_minor)) => tag_major == major && tag_minor <= minor,
        _ => false,
    }
}

/// Parse `cp314` / `py310` into `(prefix, 3, 14)` / `(prefix, 3, 10)`.
fn parse_python_tag(tag: &str) -> Option<(&str, u32, u32)> {
    let (prefix, digits) = tag
        .strip_prefix("cp")
        .map(|digits| ("cp", digits))
        .or_else(|| tag.strip_prefix("py").map(|digits| ("py", digits)))?;
    if digits.len() < 2 || !digits.chars().all(|character| character.is_ascii_digit()) {
        return None;
    }
    let major = digits[..1].parse::<u32>().ok()?;
    let minor = digits[1..].parse::<u32>().ok()?;
    Some((prefix, major, minor))
}

fn native_platform_matches(tag: &str) -> bool {
    let arch = wheel_arch();
    match env::consts::OS {
        "linux" => {
            if tag == format!("linux_{arch}").as_str() {
                return true;
            }
            if tag.starts_with("manylinux") && tag.ends_with(format!("_{arch}").as_str()) {
                return cfg!(target_env = "gnu");
            }
            if tag.starts_with("musllinux") && tag.ends_with(format!("_{arch}").as_str()) {
                return cfg!(target_env = "musl");
            }
            false
        }
        "macos" => {
            if !tag.starts_with("macosx_") {
                return false;
            }
            tag.ends_with("_universal2") || tag.ends_with(format!("_{arch}").as_str())
        }
        "windows" => {
            let expected = match env::consts::ARCH {
                "x86_64" => "win_amd64",
                "x86" => "win32",
                "aarch64" => "win_arm64",
                _ => return false,
            };
            tag == expected
        }
        _ => tag == format!("{}_{arch}", env::consts::OS).as_str(),
    }
}

/// Hydro-native wheels exist only for Linux x86_64: `linux_x86_64` or a
/// `manylinux*_x86_64` tag. `any` is never a Hydro platform. The documented
/// spelling may prefix the platform with the ABI token
/// (`hydro1_0_linux_x86_64`); the prefix is stripped first.
fn hydro_platform_tag_matches(tag: &str) -> bool {
    match split_hydro_abi(tag) {
        Some((_, Some(platform))) => hydro_platform_matches(platform),
        Some((_, None)) => false,
        None => hydro_platform_matches(tag),
    }
}

fn hydro_platform_matches(tag: &str) -> bool {
    if !(cfg!(target_os = "linux") && cfg!(target_arch = "x86_64")) {
        return false;
    }
    tag == "linux_x86_64" || (tag.starts_with("manylinux") && tag.ends_with("_x86_64"))
}

fn wheel_arch() -> &'static str {
    match env::consts::ARCH {
        "x86" => "i686",
        "arm" => "armv7l",
        "powerpc64" => {
            if cfg!(target_endian = "little") {
                "ppc64le"
            } else {
                "ppc64"
            }
        }
        "aarch64" if env::consts::OS == "macos" => "arm64",
        other => other,
    }
}

// --- installation ----------------------------------------------------------

struct InstallSummary {
    files: usize,
}

fn install_archive(archive: &ZipArchive, target: &Path) -> Result<InstallSummary, CliError> {
    if target.as_os_str().is_empty() {
        return Err(install_error("the target directory is empty"));
    }
    if target
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(install_error(format!(
            "target '{}' must not contain '..' components",
            target.display()
        )));
    }
    if target.is_absolute() && target.parent().is_none() {
        return Err(install_error(format!(
            "refusing to install into the filesystem root '{}'",
            target.display()
        )));
    }
    if target.exists() {
        if !target.is_dir() {
            return Err(install_error(format!(
                "target '{}' exists and is not a directory",
                target.display()
            )));
        }
        let canonical = fs::canonicalize(target).map_err(|error| {
            install_error(format!(
                "cannot resolve target '{}': {error}",
                target.display()
            ))
        })?;
        if canonical.parent().is_none() {
            return Err(install_error(format!(
                "refusing to install into the filesystem root '{}'",
                canonical.display()
            )));
        }
    } else if target.parent().is_none() {
        return Err(install_error(format!(
            "refusing to install into the filesystem root '{}'",
            target.display()
        )));
    }

    let parent = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|error| {
        install_error(format!(
            "cannot create target parent '{}': {error}",
            parent.display()
        ))
    })?;

    let staging = unique_staging_dir(parent)?;
    fs::create_dir(&staging).map_err(|error| {
        install_error(format!(
            "cannot create staging directory '{}': {error}",
            staging.display()
        ))
    })?;

    let result = stage_and_install(archive, target, &staging);
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

fn stage_and_install(
    archive: &ZipArchive,
    target: &Path,
    staging: &Path,
) -> Result<InstallSummary, CliError> {
    let mut files = 0usize;
    for member in &archive.entries {
        let destination = staging.join(&member.path);
        if member.is_dir {
            fs::create_dir_all(&destination).map_err(|error| {
                install_error(format!(
                    "cannot create directory '{}' from '{}': {error}",
                    destination.display(),
                    member.name
                ))
            })?;
            continue;
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                install_error(format!(
                    "cannot create directory '{}' for '{}': {error}",
                    parent.display(),
                    member.name
                ))
            })?;
        }
        let contents = archive.read_member(member)?;
        fs::write(&destination, &contents).map_err(|error| {
            install_error(format!(
                "cannot write '{}' from '{}': {error}",
                destination.display(),
                member.name
            ))
        })?;
        set_file_mode(&destination, member.mode)?;
        files += 1;
    }

    // The optional installation record is metadata, not a wheel member, so it
    // is not part of the reported unpacked file count.
    let _wrote_record = record_installer(staging)?;
    move_into_place(staging, target)?;
    Ok(InstallSummary { files })
}

fn unique_staging_dir(parent: &Path) -> Result<PathBuf, CliError> {
    let sequence = STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    for attempt in 0..100 {
        let candidate = parent.join(format!(
            "{STAGING_PREFIX}{}-{nanos}-{sequence}-{attempt}",
            std::process::id()
        ));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(install_error(format!(
        "cannot find a free staging directory below '{}'",
        parent.display()
    )))
}

/// Write `.dist-info/INSTALLER` unless the wheel already provides one.
fn record_installer(staging: &Path) -> Result<bool, CliError> {
    let mut dist_info: Option<PathBuf> = None;
    let entries = fs::read_dir(staging).map_err(|error| {
        install_error(format!(
            "cannot inspect staging directory '{}': {error}",
            staging.display()
        ))
    })?;
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            install_error(format!(
                "cannot inspect staging directory '{}': {error}",
                staging.display()
            ))
        })?;
        let file_type = entry.file_type().map_err(|error| {
            install_error(format!(
                "cannot inspect '{}': {error}",
                entry.path().display()
            ))
        })?;
        if file_type.is_dir() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".dist-info") {
                names.push(name);
            }
        }
    }
    names.sort();
    if let Some(name) = names.first() {
        dist_info = Some(staging.join(name));
    }
    let Some(dist_info) = dist_info else {
        return Ok(false);
    };
    let installer = dist_info.join("INSTALLER");
    if installer.exists() {
        return Ok(false);
    }
    fs::write(&installer, INSTALLER_CONTENT).map_err(|error| {
        install_error(format!(
            "cannot write installation record '{}': {error}",
            installer.display()
        ))
    })?;
    Ok(true)
}

fn set_file_mode(path: &Path, mode: u32) -> Result<(), CliError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if mode & 0o777 != 0 {
            fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o777)).map_err(
                |error| {
                    install_error(format!(
                        "cannot set permissions on '{}': {error}",
                        path.display()
                    ))
                },
            )?;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

/// Rename the fully staged tree into place. A new target is installed with one
/// rename; an existing target is merged with per-file renames.
fn move_into_place(staging: &Path, target: &Path) -> Result<(), CliError> {
    if !target.exists() {
        match fs::rename(staging, target) {
            Ok(()) => return Ok(()),
            Err(error) => {
                if !target.is_dir() {
                    return Err(install_error(format!(
                        "cannot move staged wheel into '{}': {error}",
                        target.display()
                    )));
                }
                // The target appeared concurrently; fall through to the merge.
            }
        }
    }
    merge_tree(staging, target)
}

fn merge_tree(staging: &Path, target: &Path) -> Result<(), CliError> {
    fs::create_dir_all(target).map_err(|error| {
        install_error(format!(
            "cannot create target '{}': {error}",
            target.display()
        ))
    })?;
    merge_directory(staging, target)?;
    let _ = fs::remove_dir_all(staging);
    Ok(())
}

fn merge_directory(source: &Path, destination: &Path) -> Result<(), CliError> {
    let entries = fs::read_dir(source).map_err(|error| {
        install_error(format!(
            "cannot read staging directory '{}': {error}",
            source.display()
        ))
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            install_error(format!(
                "cannot read staging directory '{}': {error}",
                source.display()
            ))
        })?;
        let from = entry.path();
        let to = destination.join(entry.file_name());
        let file_type = entry.file_type().map_err(|error| {
            install_error(format!("cannot inspect '{}': {error}", from.display()))
        })?;
        if file_type.is_dir() {
            if to.exists() && !to.is_dir() {
                return Err(install_error(format!(
                    "cannot install directory '{}': a file already exists there",
                    to.display()
                )));
            }
            fs::create_dir_all(&to).map_err(|error| {
                install_error(format!(
                    "cannot create directory '{}': {error}",
                    to.display()
                ))
            })?;
            merge_directory(&from, &to)?;
            continue;
        }
        if to.is_dir() {
            return Err(install_error(format!(
                "cannot install file '{}': a directory already exists there",
                to.display()
            )));
        }
        replace_file(&from, &to)?;
    }
    Ok(())
}

fn replace_file(source: &Path, destination: &Path) -> Result<(), CliError> {
    #[cfg(windows)]
    {
        if destination.exists() {
            fs::remove_file(destination).map_err(|error| {
                install_error(format!(
                    "cannot replace '{}': {error}",
                    destination.display()
                ))
            })?;
        }
    }
    fs::rename(source, destination).map_err(|error| {
        install_error(format!(
            "cannot install '{}': {error}; the target may be partially updated",
            destination.display()
        ))
    })
}

// --- wheel ZIP reader ------------------------------------------------------

struct Entry {
    name: String,
    path: PathBuf,
    is_dir: bool,
    mode: u32,
    method: u16,
    crc32: u32,
    compressed_size: u64,
    uncompressed_size: u64,
    local_offset: u64,
}

struct ZipArchive {
    data: Vec<u8>,
    entries: Vec<Entry>,
}

impl ZipArchive {
    fn open(path: &Path) -> Result<ZipArchive, CliError> {
        let data = fs::read(path).map_err(|error| {
            install_error(format!("cannot read wheel '{}': {error}", path.display()))
        })?;
        let end = find_end_record(&data)
            .ok_or_else(|| invalid_wheel("missing end-of-central-directory record"))?;
        let directory = parse_end_record(&data, end)?;
        let directory_start = usize::try_from(directory.offset)
            .map_err(|_| invalid_wheel("central directory offset does not fit this platform"))?;
        let directory_size = usize::try_from(directory.size)
            .map_err(|_| invalid_wheel("central directory size does not fit this platform"))?;
        let directory_end = directory_start
            .checked_add(directory_size)
            .ok_or_else(|| invalid_wheel("central directory size overflows"))?;
        let directory_data = data
            .get(directory_start..directory_end)
            .ok_or_else(|| invalid_wheel("central directory is outside the archive"))?;
        let entries = parse_central_directory(directory_data, directory.entries)?;
        Ok(ZipArchive { data, entries })
    }

    fn read_member(&self, entry: &Entry) -> Result<Vec<u8>, CliError> {
        let offset = usize::try_from(entry.local_offset)
            .map_err(|_| invalid_wheel(format!("member '{}' offset is too large", entry.name)))?;
        let header = slice_at(&self.data, offset, 30)?;
        if read_u32(header, 0)? != 0x0403_4b50 {
            return Err(invalid_wheel(format!(
                "member '{}' has an invalid local header",
                entry.name
            )));
        }
        let flags = read_u16(header, 6)?;
        let method = read_u16(header, 8)?;
        let name_length = read_u16(header, 26)? as usize;
        let extra_length = read_u16(header, 28)? as usize;
        if flags & 0x0001 != 0 || flags & 0x0040 != 0 {
            return Err(invalid_wheel(format!(
                "member '{}' is encrypted",
                entry.name
            )));
        }
        if method != entry.method {
            return Err(invalid_wheel(format!(
                "member '{}' has inconsistent compression methods",
                entry.name
            )));
        }
        let name_start = offset
            .checked_add(30)
            .ok_or_else(|| invalid_wheel("local header offset overflows"))?;
        let name = slice_at(&self.data, name_start, name_length)?;
        if name != entry.name.as_bytes() {
            return Err(invalid_wheel(format!(
                "member '{}' has a mismatched local name",
                entry.name
            )));
        }
        let data_start = name_start
            .checked_add(name_length)
            .and_then(|start| start.checked_add(extra_length))
            .ok_or_else(|| invalid_wheel("member data offset overflows"))?;
        let compressed_length = usize::try_from(entry.compressed_size)
            .map_err(|_| invalid_wheel(format!("member '{}' is too large", entry.name)))?;
        let compressed = slice_at(&self.data, data_start, compressed_length)?;

        let expected = usize::try_from(entry.uncompressed_size)
            .map_err(|_| invalid_wheel(format!("member '{}' is too large", entry.name)))?;
        let contents = match entry.method {
            0 => {
                if entry.compressed_size != entry.uncompressed_size {
                    return Err(invalid_wheel(format!(
                        "stored member '{}' declares different sizes",
                        entry.name
                    )));
                }
                compressed.to_vec()
            }
            8 => inflate(compressed, expected).map_err(|reason| {
                invalid_wheel(format!(
                    "cannot decompress member '{}': {reason}",
                    entry.name
                ))
            })?,
            other => {
                return Err(invalid_wheel(format!(
                    "member '{}' uses unsupported compression method {other}",
                    entry.name
                )))
            }
        };
        if contents.len() != expected {
            return Err(invalid_wheel(format!(
                "member '{}' decompressed to {} bytes, expected {expected}",
                entry.name,
                contents.len()
            )));
        }
        if crc32(&contents) != entry.crc32 {
            return Err(invalid_wheel(format!(
                "member '{}' failed its CRC check",
                entry.name
            )));
        }
        Ok(contents)
    }
}

struct EndRecord {
    entries: u64,
    size: u64,
    offset: u64,
}

fn find_end_record(data: &[u8]) -> Option<usize> {
    if data.len() < 22 {
        return None;
    }
    let start = data.len().saturating_sub(65_557);
    let tail = &data[start..];
    for position in (0..=tail.len().saturating_sub(4)).rev() {
        if &tail[position..position + 4] != b"PK\x05\x06" {
            continue;
        }
        let absolute = start + position;
        if absolute + 22 > data.len() {
            continue;
        }
        let Some(comment) = data.get(absolute + 20..absolute + 22) else {
            continue;
        };
        let comment_length = u16::from_le_bytes([comment[0], comment[1]]) as usize;
        if absolute + 22 + comment_length == data.len() {
            return Some(absolute);
        }
    }
    None
}

fn parse_end_record(data: &[u8], offset: usize) -> Result<EndRecord, CliError> {
    let record = slice_at(data, offset, 22)?;
    if read_u32(record, 0)? != 0x0605_4b50 {
        return Err(invalid_wheel("invalid end-of-central-directory record"));
    }
    let disk = read_u16(record, 4)?;
    let directory_disk = read_u16(record, 6)?;
    let entries_disk = read_u16(record, 8)? as u64;
    let entries = read_u16(record, 10)? as u64;
    let size = read_u32(record, 12)? as u64;
    let directory_offset = read_u32(record, 16)? as u64;
    if disk != 0 || directory_disk != 0 || entries_disk != entries {
        return Err(invalid_wheel("multi-disk ZIP archives are not supported"));
    }
    let has_zip64_locator = offset >= 20
        && data
            .get(offset - 20..offset - 16)
            .map(|signature| signature == b"PK\x06\x07")
            .unwrap_or(false);
    if size == 0xFFFF_FFFF
        || directory_offset == 0xFFFF_FFFF
        || (entries == 0xFFFF && has_zip64_locator)
    {
        return parse_zip64_end_record(data, offset);
    }
    Ok(EndRecord {
        entries,
        size,
        offset: directory_offset,
    })
}

fn parse_zip64_end_record(data: &[u8], end_offset: usize) -> Result<EndRecord, CliError> {
    let locator_offset = end_offset
        .checked_sub(20)
        .ok_or_else(|| invalid_wheel("missing ZIP64 locator"))?;
    let locator = slice_at(data, locator_offset, 20)?;
    if read_u32(locator, 0)? != 0x0706_4b50 {
        return Err(invalid_wheel("missing ZIP64 locator"));
    }
    let locator_disk = read_u32(locator, 4)?;
    let zip64_offset = read_u64(locator, 8)?;
    let total_disks = read_u32(locator, 16)?;
    if locator_disk != 0 || total_disks != 1 {
        return Err(invalid_wheel("multi-disk ZIP archives are not supported"));
    }
    let zip64_offset = usize::try_from(zip64_offset)
        .map_err(|_| invalid_wheel("ZIP64 record offset does not fit this platform"))?;
    let record = slice_at(data, zip64_offset, 56)?;
    if read_u32(record, 0)? != 0x0606_4b50 {
        return Err(invalid_wheel(
            "invalid ZIP64 end-of-central-directory record",
        ));
    }
    let record_size = read_u64(record, 4)?;
    if record_size < 44 {
        return Err(invalid_wheel(
            "truncated ZIP64 end-of-central-directory record",
        ));
    }
    let disk = read_u32(record, 16)?;
    let directory_disk = read_u32(record, 20)?;
    let entries_disk = read_u64(record, 24)?;
    let entries = read_u64(record, 32)?;
    let size = read_u64(record, 40)?;
    let directory_offset = read_u64(record, 48)?;
    if disk != 0 || directory_disk != 0 || entries_disk != entries {
        return Err(invalid_wheel("multi-disk ZIP archives are not supported"));
    }
    Ok(EndRecord {
        entries,
        size,
        offset: directory_offset,
    })
}

fn parse_central_directory(data: &[u8], count: u64) -> Result<Vec<Entry>, CliError> {
    let mut entries = Vec::new();
    let mut cursor = 0usize;
    for _ in 0..count {
        let header = slice_at(data, cursor, 46)?;
        if read_u32(header, 0)? != 0x0201_4b50 {
            return Err(invalid_wheel("invalid central directory entry"));
        }
        let flags = read_u16(header, 8)?;
        let method = read_u16(header, 10)?;
        let crc = read_u32(header, 16)?;
        let mut compressed_size = read_u32(header, 20)? as u64;
        let mut uncompressed_size = read_u32(header, 24)? as u64;
        let name_length = read_u16(header, 28)? as usize;
        let extra_length = read_u16(header, 30)? as usize;
        let comment_length = read_u16(header, 32)? as usize;
        let disk_start = read_u16(header, 34)? as u32;
        let external_attributes = read_u32(header, 38)?;
        let mut local_offset = read_u32(header, 42)? as u64;

        if flags & 0x0001 != 0 || flags & 0x0040 != 0 {
            return Err(invalid_wheel("encrypted wheel members are not supported"));
        }

        let name_start = cursor
            .checked_add(46)
            .ok_or_else(|| invalid_wheel("central directory offset overflows"))?;
        let name_bytes = slice_at(data, name_start, name_length)?;
        let name = std::str::from_utf8(name_bytes)
            .map_err(|_| invalid_wheel("wheel member name is not valid UTF-8"))?
            .to_string();
        if name.is_empty() {
            return Err(invalid_wheel("wheel member has an empty name"));
        }
        let extra_start = name_start
            .checked_add(name_length)
            .ok_or_else(|| invalid_wheel("central directory offset overflows"))?;
        let extra = slice_at(data, extra_start, extra_length)?;
        let (uncompressed_size_after, compressed_size_after, offset_after, disk_after) =
            apply_zip64_extra(
                extra,
                uncompressed_size,
                compressed_size,
                local_offset,
                disk_start,
            )?;
        uncompressed_size = uncompressed_size_after;
        compressed_size = compressed_size_after;
        local_offset = offset_after;
        if disk_after != 0 {
            return Err(invalid_wheel("multi-disk ZIP archives are not supported"));
        }

        match method {
            0 | 8 => {}
            other => {
                return Err(invalid_wheel(format!(
                    "member '{name}' uses unsupported compression method {other}"
                )))
            }
        }

        let mode = external_attributes >> 16;
        let file_type = mode & 0o170000;
        if file_type == 0o120000 {
            return Err(invalid_wheel(format!(
                "member '{name}' is a symlink; symlinks are not installed"
            )));
        }
        if file_type != 0 && file_type != 0o100000 && file_type != 0o040000 {
            return Err(invalid_wheel(format!(
                "member '{name}' has an unsupported file type"
            )));
        }
        let path = member_path(&name)?;
        let is_dir = name.ends_with('/') || file_type == 0o040000;

        entries.push(Entry {
            name,
            path,
            is_dir,
            mode,
            method,
            crc32: crc,
            compressed_size,
            uncompressed_size,
            local_offset,
        });

        cursor = extra_start
            .checked_add(extra_length)
            .and_then(|position| position.checked_add(comment_length))
            .ok_or_else(|| invalid_wheel("central directory offset overflows"))?;
    }
    validate_members(&entries)?;
    Ok(entries)
}

fn apply_zip64_extra(
    extra: &[u8],
    uncompressed_size: u64,
    compressed_size: u64,
    local_offset: u64,
    disk_start: u32,
) -> Result<(u64, u64, u64, u32), CliError> {
    let mut uncompressed_size = uncompressed_size;
    let mut compressed_size = compressed_size;
    let mut local_offset = local_offset;
    let mut disk_start = disk_start;
    let mut position = 0usize;
    while position + 4 <= extra.len() {
        let id = read_u16(extra, position)?;
        let size = read_u16(extra, position + 2)? as usize;
        let body_start = position + 4;
        let body = slice_at(extra, body_start, size)?;
        let mut body_position = 0usize;
        if id == 0x0001 {
            if uncompressed_size == 0xFFFF_FFFF {
                uncompressed_size = read_u64(body, body_position)?;
                body_position += 8;
            }
            if compressed_size == 0xFFFF_FFFF {
                compressed_size = read_u64(body, body_position)?;
                body_position += 8;
            }
            if local_offset == 0xFFFF_FFFF {
                local_offset = read_u64(body, body_position)?;
                body_position += 8;
            }
            if disk_start == 0xFFFF {
                disk_start = read_u32(body, body_position)?;
            }
        }
        position = body_start
            .checked_add(size)
            .ok_or_else(|| invalid_wheel("ZIP extra field overflows"))?;
    }
    Ok((uncompressed_size, compressed_size, local_offset, disk_start))
}

fn validate_members(entries: &[Entry]) -> Result<(), CliError> {
    let mut seen: HashSet<&Path> = HashSet::new();
    for entry in entries {
        if !seen.insert(entry.path.as_path()) {
            return Err(invalid_wheel(format!(
                "duplicate wheel member '{}'",
                entry.name
            )));
        }
    }
    let files: HashSet<PathBuf> = entries
        .iter()
        .filter(|entry| !entry.is_dir)
        .map(|entry| entry.path.clone())
        .collect();
    for entry in entries {
        for ancestor in entry.path.ancestors().skip(1) {
            if ancestor.as_os_str().is_empty() {
                continue;
            }
            if files.contains(ancestor) {
                return Err(invalid_wheel(format!(
                    "member '{}' is nested below file '{}'",
                    entry.name,
                    ancestor.display()
                )));
            }
        }
    }
    Ok(())
}

/// Validate and normalize a member name, rejecting traversal and absolute paths.
fn member_path(name: &str) -> Result<PathBuf, CliError> {
    if name.starts_with('/') || name.starts_with('\\') {
        return Err(invalid_wheel(format!(
            "member '{name}' has an absolute path"
        )));
    }
    if name.contains('\\') {
        return Err(invalid_wheel(format!(
            "member '{name}' contains a backslash"
        )));
    }
    if name.contains('\0') {
        return Err(invalid_wheel(format!(
            "member '{name}' contains a NUL byte"
        )));
    }
    let trimmed = name.strip_suffix('/').unwrap_or(name);
    if trimmed.is_empty() {
        return Err(invalid_wheel("wheel member has an empty name"));
    }
    if trimmed.as_bytes().get(1) == Some(&b':') {
        return Err(invalid_wheel(format!(
            "member '{name}' has a drive-letter path"
        )));
    }
    let mut path = PathBuf::new();
    let mut depth = 0usize;
    for component in trimmed.split('/') {
        match component {
            "" => {
                return Err(invalid_wheel(format!(
                    "member '{name}' has an empty path component"
                )))
            }
            "." => {
                return Err(invalid_wheel(format!(
                    "member '{name}' has a '.' path component"
                )))
            }
            ".." => {
                return Err(invalid_wheel(format!(
                    "member '{name}' contains a '..' component (path traversal) \
                     that escapes the target directory"
                )))
            }
            component if component.contains(':') => {
                return Err(invalid_wheel(format!(
                    "member '{name}' has a component containing ':'"
                )))
            }
            component => {
                depth += 1;
                if depth > 256 {
                    return Err(invalid_wheel(format!(
                        "member '{name}' is nested too deeply"
                    )));
                }
                path.push(component);
            }
        }
    }
    if path.is_absolute() {
        return Err(invalid_wheel(format!(
            "member '{name}' has an absolute path"
        )));
    }
    Ok(path)
}

fn slice_at(data: &[u8], offset: usize, length: usize) -> Result<&[u8], CliError> {
    let end = offset
        .checked_add(length)
        .ok_or_else(|| invalid_wheel("ZIP structure offset overflows"))?;
    data.get(offset..end)
        .ok_or_else(|| invalid_wheel("truncated ZIP structure"))
}

fn read_u16(data: &[u8], offset: usize) -> Result<u16, CliError> {
    let bytes = slice_at(data, offset, 2)?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn read_u32(data: &[u8], offset: usize) -> Result<u32, CliError> {
    let bytes = slice_at(data, offset, 4)?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_u64(data: &[u8], offset: usize) -> Result<u64, CliError> {
    let bytes = slice_at(data, offset, 8)?;
    Ok(u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]))
}

// --- deflate (RFC 1951) ----------------------------------------------------

const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DISTANCE_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DISTANCE_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

struct BitReader<'a> {
    data: &'a [u8],
    byte: usize,
    bit: u32,
}

impl<'a> BitReader<'a> {
    fn bits(&mut self, count: u32) -> Result<u32, String> {
        let mut value = 0u32;
        for index in 0..count {
            let byte = *self
                .data
                .get(self.byte)
                .ok_or_else(|| "truncated deflate stream".to_string())?;
            let bit = (byte >> self.bit) & 1;
            value |= (bit as u32) << index;
            self.bit += 1;
            if self.bit == 8 {
                self.bit = 0;
                self.byte += 1;
            }
        }
        Ok(value)
    }

    fn align(&mut self) {
        if self.bit != 0 {
            self.bit = 0;
            self.byte += 1;
        }
    }

    fn bytes(&mut self, count: usize) -> Result<&'a [u8], String> {
        debug_assert_eq!(self.bit, 0);
        let end = self
            .byte
            .checked_add(count)
            .ok_or_else(|| "deflate stream offset overflows".to_string())?;
        let slice = self
            .data
            .get(self.byte..end)
            .ok_or_else(|| "truncated deflate stream".to_string())?;
        self.byte = end;
        Ok(slice)
    }
}

struct Huffman {
    counts: [u16; 16],
    symbols: Vec<u16>,
}

impl Huffman {
    fn new(lengths: &[u8]) -> Result<Huffman, String> {
        let mut counts = [0u16; 16];
        for &length in lengths {
            if length > 15 {
                return Err("invalid Huffman code length".to_string());
            }
            counts[length as usize] += 1;
        }
        let mut remaining = 1i32;
        for &count in counts.iter().skip(1) {
            remaining <<= 1;
            remaining -= count as i32;
            if remaining < 0 {
                return Err("over-subscribed Huffman code".to_string());
            }
        }
        let mut offsets = [0u16; 16];
        for length in 1..15 {
            offsets[length + 1] = offsets[length] + counts[length];
        }
        let mut symbols = vec![0u16; lengths.len()];
        for (symbol, &length) in lengths.iter().enumerate() {
            if length != 0 {
                let index = offsets[length as usize] as usize;
                if index >= symbols.len() {
                    return Err("invalid Huffman symbol table".to_string());
                }
                symbols[index] = symbol as u16;
                offsets[length as usize] += 1;
            }
        }
        Ok(Huffman { counts, symbols })
    }

    fn decode(&self, reader: &mut BitReader<'_>) -> Result<u16, String> {
        let mut code = 0i32;
        let mut first = 0i32;
        let mut index = 0i32;
        for length in 1..16 {
            code |= reader.bits(1)? as i32;
            let count = self.counts[length] as i32;
            if code - first < count {
                let position = index + (code - first);
                return self
                    .symbols
                    .get(position as usize)
                    .copied()
                    .ok_or_else(|| "invalid Huffman code".to_string());
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err("invalid Huffman code".to_string())
    }
}

fn inflate(input: &[u8], expected: usize) -> Result<Vec<u8>, String> {
    // Do not trust the declared size for the initial allocation.
    let mut output = Vec::with_capacity(expected.min(1 << 20));
    let mut reader = BitReader {
        data: input,
        byte: 0,
        bit: 0,
    };
    loop {
        let final_block = reader.bits(1)? == 1;
        let block_type = reader.bits(2)?;
        match block_type {
            0 => inflate_stored(&mut reader, &mut output, expected)?,
            1 => {
                let (literals, distances) = fixed_tables()?;
                inflate_block(&mut reader, &mut output, expected, &literals, &distances)?;
            }
            2 => {
                let (literals, distances) = dynamic_tables(&mut reader)?;
                inflate_block(&mut reader, &mut output, expected, &literals, &distances)?;
            }
            _ => return Err("invalid deflate block type".to_string()),
        }
        if final_block {
            break;
        }
    }
    if output.len() != expected {
        return Err(format!(
            "deflated size {} does not match the declared size {expected}",
            output.len()
        ));
    }
    Ok(output)
}

fn inflate_stored(
    reader: &mut BitReader<'_>,
    output: &mut Vec<u8>,
    expected: usize,
) -> Result<(), String> {
    reader.align();
    let length = reader.bits(16)? as usize;
    let complement = reader.bits(16)? as usize;
    if length != (!complement & 0xFFFF) {
        return Err("stored block length check failed".to_string());
    }
    if output.len() + length > expected {
        return Err("deflate output exceeds the declared size".to_string());
    }
    let bytes = reader.bytes(length)?;
    output.extend_from_slice(bytes);
    Ok(())
}

fn inflate_block(
    reader: &mut BitReader<'_>,
    output: &mut Vec<u8>,
    expected: usize,
    literals: &Huffman,
    distances: &Huffman,
) -> Result<(), String> {
    loop {
        let symbol = literals.decode(reader)?;
        match symbol {
            0..=255 => {
                if output.len() >= expected {
                    return Err("deflate output exceeds the declared size".to_string());
                }
                output.push(symbol as u8);
            }
            256 => return Ok(()),
            257..=285 => {
                let index = (symbol - 257) as usize;
                let length =
                    LENGTH_BASE[index] as usize + reader.bits(LENGTH_EXTRA[index] as u32)? as usize;
                let distance_symbol = distances.decode(reader)? as usize;
                if distance_symbol >= DISTANCE_BASE.len() {
                    return Err("invalid deflate distance symbol".to_string());
                }
                let distance = DISTANCE_BASE[distance_symbol] as usize
                    + reader.bits(DISTANCE_EXTRA[distance_symbol] as u32)? as usize;
                if distance == 0 || distance > output.len() {
                    return Err("invalid deflate distance".to_string());
                }
                if output.len() + length > expected {
                    return Err("deflate output exceeds the declared size".to_string());
                }
                for _ in 0..length {
                    let byte = output[output.len() - distance];
                    output.push(byte);
                }
            }
            _ => return Err(format!("invalid deflate literal/length symbol {symbol}")),
        }
    }
}

fn fixed_tables() -> Result<(Huffman, Huffman), String> {
    let mut literal_lengths = [0u8; 288];
    for length in literal_lengths.iter_mut().take(144) {
        *length = 8;
    }
    for length in literal_lengths.iter_mut().take(256).skip(144) {
        *length = 9;
    }
    for length in literal_lengths.iter_mut().take(280).skip(256) {
        *length = 7;
    }
    for length in literal_lengths.iter_mut().take(288).skip(280) {
        *length = 8;
    }
    let literal_tree = Huffman::new(&literal_lengths)?;
    let distance_tree = Huffman::new(&[5u8; 32])?;
    Ok((literal_tree, distance_tree))
}

fn dynamic_tables(reader: &mut BitReader<'_>) -> Result<(Huffman, Huffman), String> {
    let literal_count = reader.bits(5)? as usize + 257;
    let distance_count = reader.bits(5)? as usize + 1;
    let code_count = reader.bits(4)? as usize + 4;
    if literal_count > 286 || distance_count > 30 {
        return Err("invalid dynamic Huffman counts".to_string());
    }
    const ORDER: [usize; 19] = [
        16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
    ];
    let mut code_lengths = [0u8; 19];
    for &index in ORDER.iter().take(code_count) {
        code_lengths[index] = reader.bits(3)? as u8;
    }
    let code_tree = Huffman::new(&code_lengths)?;
    let total = literal_count + distance_count;
    let mut lengths = Vec::with_capacity(total);
    while lengths.len() < total {
        let symbol = code_tree.decode(reader)?;
        match symbol {
            0..=15 => lengths.push(symbol as u8),
            16 => {
                let previous = *lengths
                    .last()
                    .ok_or_else(|| "repeat code without a previous length".to_string())?;
                let repeat = 3 + reader.bits(2)? as usize;
                if lengths.len() + repeat > total {
                    return Err("code length repeat exceeds the table".to_string());
                }
                for _ in 0..repeat {
                    lengths.push(previous);
                }
            }
            17 => {
                let repeat = 3 + reader.bits(3)? as usize;
                if lengths.len() + repeat > total {
                    return Err("code length repeat exceeds the table".to_string());
                }
                lengths.resize(lengths.len() + repeat, 0);
            }
            18 => {
                let repeat = 11 + reader.bits(7)? as usize;
                if lengths.len() + repeat > total {
                    return Err("code length repeat exceeds the table".to_string());
                }
                lengths.resize(lengths.len() + repeat, 0);
            }
            _ => return Err("invalid code length symbol".to_string()),
        }
    }
    if lengths[256] == 0 {
        return Err("dynamic block has no end-of-block code".to_string());
    }
    let literal_tree = Huffman::new(&lengths[..literal_count])?;
    let distance_tree = Huffman::new(&lengths[literal_count..])?;
    Ok((literal_tree, distance_tree))
}

/// CRC-32 (IEEE 802.3) used by ZIP members.
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

fn install_error(message: impl Into<String>) -> CliError {
    CliError::new(1, format!("tea: install: {}", message.into()))
}

fn invalid_wheel(message: impl Into<String>) -> CliError {
    install_error(format!("invalid wheel: {}", message.into()))
}
