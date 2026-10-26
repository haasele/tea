//! Flavor profiles: persistent JIT profile counts used by AOT builds.
//!
//! `tea steep` records the counts collected during a JIT run in
//! `.tea/flavors/<name>.json`; `tea brew --flavor <name>` reads them back and
//! passes the hot functions to the AOT code generator. The file format is a
//! small JSON document:
//!
//! ```json
//! {"version":1,"flavor":"default","runs":2,"functions":{"foo":4}}
//! ```
//!
//! The module is `std`-only: the schema is tiny enough that a hand-written
//! parser is smaller than a JSON dependency would be.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process;

use crate::cli::CliError;

/// Directory that stores flavor profiles, relative to the working directory.
pub(crate) const FLAVOR_DIR: &str = ".tea/flavors";
/// Flavor used when `--flavor` is omitted.
pub(crate) const DEFAULT_FLAVOR: &str = "default";

const VERSION: u64 = 1;
const DUMP_HEADER: &str = "# hydro-profile-v1";

/// One stored flavor: how often it ran and how often each function ran.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Flavor {
    pub(crate) name: String,
    pub(crate) runs: u64,
    pub(crate) functions: BTreeMap<String, u64>,
}

impl Flavor {
    pub(crate) fn new(name: &str) -> Flavor {
        Flavor {
            name: name.to_string(),
            runs: 0,
            functions: BTreeMap::new(),
        }
    }

    pub(crate) fn function_count(&self) -> usize {
        self.functions.len()
    }

    /// The counts in the shape the code generator consumes.
    pub(crate) fn hot_functions(&self) -> std::collections::HashMap<String, u64> {
        self.functions
            .iter()
            .map(|(name, count)| (name.clone(), *count))
            .collect()
    }

    /// Adds one profiling run with the counts of its dump.
    pub(crate) fn record_run(&mut self, counts: &[(String, u64)]) {
        self.runs = self.runs.saturating_add(1);
        for (name, count) in counts {
            let entry = self.functions.entry(name.clone()).or_insert(0);
            *entry = entry.saturating_add(*count);
        }
    }

    /// Sums another flavor's runs and function counts into this one.
    pub(crate) fn merge(&mut self, other: &Flavor) {
        self.runs = self.runs.saturating_add(other.runs);
        for (name, count) in &other.functions {
            let entry = self.functions.entry(name.clone()).or_insert(0);
            *entry = entry.saturating_add(*count);
        }
    }

    /// Writes the flavor atomically: a temporary sibling is renamed over the
    /// target, so a crash never leaves a half-written profile behind.
    pub(crate) fn save(&self) -> Result<(), CliError> {
        validate_name(&self.name).map_err(|reason| CliError::new(2, format!("tea: {reason}")))?;
        let path = flavor_path(&self.name);
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).map_err(|error| {
            CliError::new(
                2,
                format!(
                    "tea: cannot create flavor directory '{}': {error}",
                    parent.display()
                ),
            )
        })?;
        let file_name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let temporary = path.with_file_name(format!("{file_name}.{}.tmp", process::id()));
        let text = self.to_json();
        let written =
            fs::write(&temporary, text.as_bytes()).and_then(|()| fs::rename(&temporary, &path));
        if let Err(error) = written {
            let _ = fs::remove_file(&temporary);
            return Err(CliError::new(
                2,
                format!("tea: cannot write flavor '{}': {error}", path.display()),
            ));
        }
        Ok(())
    }

    fn to_json(&self) -> String {
        use std::fmt::Write as _;

        let mut out = String::with_capacity(64 + self.functions.len() * 16);
        let _ = write!(
            out,
            "{{\"version\":{VERSION},\"flavor\":{},\"runs\":{},\"functions\":{{",
            crate::cli::diag::json_string(&self.name),
            self.runs
        );
        for (index, (name, count)) in self.functions.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            let _ = write!(out, "{}:{}", crate::cli::diag::json_string(name), count);
        }
        out.push_str("}}\n");
        out
    }

    fn from_json(name: &str, text: &str) -> Result<Flavor, String> {
        let Json::Object(mut object) = parse_json(text)? else {
            return Err(String::from("expected a JSON object"));
        };
        let version = match object.remove("version") {
            Some(Json::Number(version)) => version,
            _ => return Err(String::from("missing or invalid 'version'")),
        };
        if version != VERSION {
            return Err(format!(
                "unsupported version {version} (this tea understands version {VERSION})"
            ));
        }
        let stored = match object.remove("flavor") {
            Some(Json::String(flavor)) => flavor,
            _ => return Err(String::from("missing or invalid 'flavor'")),
        };
        if stored != name {
            return Err(format!("declares flavor '{stored}', expected '{name}'"));
        }
        let runs = match object.remove("runs") {
            Some(Json::Number(runs)) => runs,
            _ => return Err(String::from("missing or invalid 'runs'")),
        };
        let mut functions = BTreeMap::new();
        match object.remove("functions") {
            Some(Json::Object(entries)) => {
                for (function, count) in entries {
                    let count = match count {
                        Json::Number(count) => count,
                        _ => return Err(format!("function '{function}' has a non-integer count")),
                    };
                    functions.insert(function, count);
                }
            }
            _ => return Err(String::from("missing or invalid 'functions'")),
        }
        Ok(Flavor {
            name: name.to_string(),
            runs,
            functions,
        })
    }
}

/// Validates a flavor name used as a file name.
///
/// Only `[A-Za-z0-9._-]` is accepted, and `.`/`..`/path separators are
/// rejected so a name can never escape `.tea/flavors`.
pub(crate) fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err(String::from("invalid flavor name '': must not be empty"));
    }
    if name == "."
        || name == ".."
        || name.contains("..")
        || name.contains('/')
        || name.contains('\\')
    {
        return Err(format!(
            "invalid flavor name '{name}': must not contain path separators or '..'"
        ));
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(format!(
            "invalid flavor name '{name}': allowed characters are letters, digits, '.', '_' and '-'"
        ));
    }
    Ok(())
}

/// Path of a stored flavor, relative to the working directory.
pub(crate) fn flavor_path(name: &str) -> PathBuf {
    Path::new(FLAVOR_DIR).join(format!("{name}.json"))
}

/// Loads one flavor; a missing file is an actionable error.
pub(crate) fn load(name: &str) -> Result<Flavor, CliError> {
    validate_name(name).map_err(|reason| CliError::new(2, format!("tea: {reason}")))?;
    let path = flavor_path(name);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(CliError::new(
                1,
                format!(
                    "tea: flavor '{name}' not found ('{}' does not exist); \
                     record it with `tea steep --flavor {name} <file.py>`",
                    path.display()
                ),
            ))
        }
        Err(error) => {
            return Err(CliError::new(
                1,
                format!("tea: cannot read flavor '{}': {error}", path.display()),
            ))
        }
    };
    Flavor::from_json(name, &text).map_err(|reason| {
        CliError::new(
            1,
            format!("tea: invalid flavor '{}': {reason}", path.display()),
        )
    })
}

/// Like [`load`], but an absent flavor starts empty (used as a merge target).
pub(crate) fn load_optional(name: &str) -> Result<Flavor, CliError> {
    validate_name(name).map_err(|reason| CliError::new(2, format!("tea: {reason}")))?;
    if flavor_path(name).is_file() {
        load(name)
    } else {
        Ok(Flavor::new(name))
    }
}

/// All readable flavors, sorted by name.
pub(crate) fn list() -> Result<Vec<Flavor>, CliError> {
    let directory = Path::new(FLAVOR_DIR);
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(CliError::new(
                1,
                format!(
                    "tea: cannot read flavor directory '{}': {error}",
                    directory.display()
                ),
            ))
        }
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            CliError::new(
                1,
                format!(
                    "tea: cannot read flavor directory '{}': {error}",
                    directory.display()
                ),
            )
        })?;
        let path = entry.path();
        if !path.is_file()
            || !path
                .extension()
                .is_some_and(|extension| extension == "json")
        {
            continue;
        }
        let Some(stem) = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
        else {
            continue;
        };
        if validate_name(&stem).is_err() {
            continue;
        }
        names.push(stem);
    }
    names.sort();
    let mut flavors = Vec::with_capacity(names.len());
    for name in &names {
        flavors.push(load(name)?);
    }
    Ok(flavors)
}

/// Parses a runtime profile dump.
///
/// The format is the header `# hydro-profile-v1`, then one
/// `<count>\t<name>` line per function. Empty lines are ignored.
pub(crate) fn parse_dump(text: &str) -> Result<Vec<(String, u64)>, CliError> {
    let mut lines = text.lines();
    let header = lines.next().unwrap_or_default().trim_end_matches('\r');
    if header != DUMP_HEADER {
        return Err(CliError::new(
            2,
            format!("tea: internal profile error: expected '{DUMP_HEADER}'"),
        ));
    }
    let mut counts = Vec::new();
    for (index, line) in lines.enumerate() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let Some((count, name)) = line.split_once('\t') else {
            return Err(CliError::new(
                2,
                format!(
                    "tea: internal profile error: malformed line {} in the profile dump",
                    index + 2
                ),
            ));
        };
        if name.is_empty() {
            return Err(CliError::new(
                2,
                format!(
                    "tea: internal profile error: empty function name on line {}",
                    index + 2
                ),
            ));
        }
        let count: u64 = count.parse().map_err(|_| {
            CliError::new(
                2,
                format!(
                    "tea: internal profile error: invalid count '{count}' on line {}",
                    index + 2
                ),
            )
        })?;
        counts.push((name.to_string(), count));
    }
    Ok(counts)
}

// ---------------------------------------------------------------------------
// Minimal JSON reader (objects, arrays, strings, unsigned integers, booleans).

enum Json {
    Number(u64),
    String(String),
    Object(BTreeMap<String, Json>),
}

fn parse_json(text: &str) -> Result<Json, String> {
    let mut parser = JsonParser {
        text,
        bytes: text.as_bytes(),
        pos: 0,
    };
    parser.skip_whitespace();
    let value = parser.value()?;
    parser.skip_whitespace();
    if parser.pos != parser.bytes.len() {
        return Err(String::from("trailing content after the JSON value"));
    }
    Ok(value)
}

struct JsonParser<'a> {
    text: &'a str,
    bytes: &'a [u8],
    pos: usize,
}

impl JsonParser<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.pos += 1;
        Some(byte)
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn expect(&mut self, expected: u8) -> Result<(), String> {
        match self.next() {
            Some(byte) if byte == expected => Ok(()),
            _ => Err(format!("expected '{}'", expected as char)),
        }
    }

    fn value(&mut self) -> Result<Json, String> {
        match self.peek() {
            Some(b'{') => self.object(),
            Some(b'"') => self.string().map(Json::String),
            Some(byte) if byte.is_ascii_digit() => self.number().map(Json::Number),
            _ => Err(String::from(
                "expected a flavor object, string or unsigned integer",
            )),
        }
    }

    fn object(&mut self) -> Result<Json, String> {
        self.expect(b'{')?;
        let mut object = BTreeMap::new();
        self.skip_whitespace();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Json::Object(object));
        }
        loop {
            self.skip_whitespace();
            if self.peek() != Some(b'"') {
                return Err(String::from("expected an object key"));
            }
            let key = self.string()?;
            self.skip_whitespace();
            self.expect(b':')?;
            self.skip_whitespace();
            let value = self.value()?;
            object.insert(key, value);
            self.skip_whitespace();
            match self.next() {
                Some(b',') => continue,
                Some(b'}') => break,
                _ => return Err(String::from("expected ',' or '}' in object")),
            }
        }
        Ok(Json::Object(object))
    }

    fn string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            let byte = self
                .next()
                .ok_or_else(|| String::from("unterminated string"))?;
            match byte {
                b'"' => return Ok(out),
                b'\\' => {
                    let escape = self
                        .next()
                        .ok_or_else(|| String::from("unterminated string escape"))?;
                    match escape {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let code = self.unicode_escape()?;
                            out.push(
                                char::from_u32(code)
                                    .ok_or_else(|| String::from("invalid unicode escape"))?,
                            );
                        }
                        _ => return Err(String::from("invalid string escape")),
                    }
                }
                byte if byte < 0x20 => return Err(String::from("control character in string")),
                byte if byte < 0x80 => out.push(byte as char),
                _ => {
                    let start = self.pos - 1;
                    let character = self.text[start..]
                        .chars()
                        .next()
                        .ok_or_else(|| String::from("invalid UTF-8 in string"))?;
                    self.pos = start + character.len_utf8();
                    out.push(character);
                }
            }
        }
    }

    fn unicode_escape(&mut self) -> Result<u32, String> {
        let first = self.hex4()?;
        if !(0xD800..0xDC00).contains(&first) {
            return Ok(first);
        }
        if self.next() != Some(b'\\') || self.next() != Some(b'u') {
            return Err(String::from("unpaired unicode surrogate"));
        }
        let second = self.hex4()?;
        if !(0xDC00..0xE000).contains(&second) {
            return Err(String::from("invalid unicode surrogate"));
        }
        Ok(0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00))
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let mut value = 0u32;
        for _ in 0..4 {
            let digit = self
                .next()
                .and_then(|byte| char::from(byte).to_digit(16))
                .ok_or_else(|| String::from("invalid unicode escape"))?;
            value = value * 16 + digit;
        }
        Ok(value)
    }

    fn number(&mut self) -> Result<u64, String> {
        let start = self.pos;
        while matches!(self.peek(), Some(byte) if byte.is_ascii_digit()) {
            self.pos += 1;
        }
        if matches!(self.peek(), Some(b'.' | b'e' | b'E' | b'+' | b'-')) {
            return Err(String::from("expected an unsigned integer"));
        }
        let text = &self.text[start..self.pos];
        text.parse::<u64>()
            .map_err(|_| String::from("integer out of range"))
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_dump, validate_name, Flavor};

    #[test]
    fn names_reject_traversal_and_separators() {
        for name in ["default", "v1.2", "a_b-c"] {
            assert!(validate_name(name).is_ok(), "{name}");
        }
        for name in ["", ".", "..", "../x", "a/b", "a\\b", "a b"] {
            assert!(validate_name(name).is_err(), "{name}");
        }
    }

    #[test]
    fn dump_parses_header_and_counts() {
        let counts =
            parse_dump("# hydro-profile-v1\n4\tfoo\n2\t<module>\n").expect("dump must parse");
        assert_eq!(
            counts,
            vec![(String::from("foo"), 4), (String::from("<module>"), 2)]
        );
    }

    #[test]
    fn dump_requires_header() {
        assert!(parse_dump("4\tfoo\n").is_err());
    }

    #[test]
    fn flavor_round_trips_json() {
        let mut flavor = Flavor::new("default");
        flavor.record_run(&[(String::from("f"), 3)]);
        let json = flavor.to_json();
        let parsed = Flavor::from_json("default", &json).expect("json must parse");
        assert_eq!(parsed, flavor);
    }
}
