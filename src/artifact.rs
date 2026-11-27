//! Bounded tool-result output: `--max-output BYTES` and `--output-dir DIR`.
//!
//! An agent driving the REPL pays for every byte a tool returns. With a
//! budget set, a tool result whose printed JSON is larger than the budget is
//! written whole to an owner-only file, and a summary object naming that file
//! is printed in its place. The summary is versioned (`"mcpRepl":
//! "truncated/v1"`) so a consumer can tell it from a `CallToolResult`.
//!
//! The rules a summary keeps:
//! - the saved file holds exactly the line that would have been printed, so
//!   reading it is the same as running without a budget;
//! - JSON is never cut: structured values are described (type, size, keys),
//!   and only text is previewed, on a character boundary;
//! - `isError` is carried over, so an error never reads as success;
//! - a file that cannot be written is reported in the summary, not dropped;
//! - the summary fits the budget whenever its fixed fields do. Optional
//!   descriptors, keys, and previews are added only while they fit. When the
//!   fixed fields alone are larger than the budget, they are printed anyway:
//!   that is the floor.
//!
//! Threat model for the output directory: it must not be replaceable by
//! another account. An explicit `--output-dir` that another account owns or
//! can write to is refused. mcp-repl does not defend against a process
//! running as the same user, which could swap the directory between the
//! check and a write. Owner-only modes are a unix guarantee; on other
//! platforms files inherit the directory's access control.
//!
//! This module measures, saves, and summarizes. Printing is the caller's.

use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Map, Value};

use crate::secure_file;

/// The `mcpRepl` marker on a summary. Bump the version on any change a
/// consumer would have to handle differently.
pub(crate) const FORMAT: &str = "truncated/v1";

/// Content blocks described in a summary at most; the rest are only counted.
const MAX_BLOCKS: usize = 32;
/// Object keys listed when describing a value.
const MAX_KEYS: usize = 32;
/// Server-provided strings (URIs, MIME types, key names) longer than this are
/// shortened or left out of a summary. The saved file has them whole.
const MAX_FIELD_BYTES: usize = 256;
/// Longest tool-name part of a saved file's name.
const MAX_STEM_CHARS: usize = 64;
/// Fresh names tried before giving up on a directory full of collisions.
const NAME_ATTEMPTS: usize = 16;
/// `,"preview":""`: what adding a preview costs beyond its escaped text.
const PREVIEW_OVERHEAD: usize = 13;

/// What was measured: decided by the dispatcher, never guessed from the
/// value's shape, so a selected object that happens to have a `content`
/// array is still described as a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Shape {
    /// A whole `CallToolResult`, as a plain tool call prints it.
    Result,
    /// A value selected by `| path` or bound by a capture.
    Value,
}

/// Where saved results go.
enum Destination {
    /// `--output-dir`, checked and created at startup.
    Explicit(PathBuf),
    /// No `--output-dir`: a private temp directory, created on first use so
    /// a run that never goes over budget leaves nothing behind.
    Default(OnceLock<Result<PathBuf, String>>),
}

/// The process-wide output budget.
pub(crate) struct Budget {
    max: usize,
    destination: Destination,
    sequence: AtomicU64,
}

/// A result that went over budget: the summary to print in its place, and
/// why the full result could not be saved, when it could not.
#[derive(Debug)]
pub(crate) struct Truncation {
    pub summary: Value,
    pub save_error: Option<String>,
}

static BUDGET: OnceLock<Budget> = OnceLock::new();

/// Install the budget for this process. Like the other per-process policies
/// it is set once at startup; a second call is ignored.
pub(crate) fn install(budget: Budget) {
    let _ = BUDGET.set(budget);
}

/// The budget, when `--max-output` was given.
pub(crate) fn active() -> Option<&'static Budget> {
    BUDGET.get()
}

impl Budget {
    /// Check `--output-dir` (creating it owner-only when missing) and build
    /// the budget. Returns a warning to print when the directory is readable
    /// by other accounts.
    pub(crate) fn new(
        max: usize,
        output_dir: Option<&Path>,
    ) -> Result<(Self, Option<String>), String> {
        let (destination, warning) = match output_dir {
            Some(dir) => {
                let (dir, warning) = prepare_dir(dir)?;
                (Destination::Explicit(dir), warning)
            }
            None => (Destination::Default(OnceLock::new()), None),
        };
        let budget = Self {
            max,
            destination,
            sequence: AtomicU64::new(0),
        };
        Ok((budget, warning))
    }

    /// Measure `value` as `--json` would print it: one compact line, without
    /// its newline. `None` when it fits. Otherwise save the line and return
    /// the summary to print instead.
    pub(crate) fn apply(
        &self,
        tool: &str,
        is_error: bool,
        shape: Shape,
        value: &Value,
    ) -> Option<Truncation> {
        let line = value.to_string();
        if line.len() <= self.max {
            return None;
        }
        let saved = self.save(tool, &line, random_suffix);
        // Lossless: both destinations are checked to be valid UTF-8, and the
        // file name is ASCII.
        let shown = saved
            .as_ref()
            .map(|path| path.to_string_lossy())
            .map_err(String::as_str);
        let summary = summarize(
            tool,
            is_error,
            shape,
            value,
            line.len(),
            self.max,
            shown.as_deref().map_err(|error| *error),
        );
        Some(Truncation {
            summary,
            save_error: saved.err(),
        })
    }

    fn directory(&self) -> Result<&Path, String> {
        match &self.destination {
            Destination::Explicit(dir) => Ok(dir),
            Destination::Default(cell) => cell
                .get_or_init(create_default_dir)
                .as_deref()
                .map_err(Clone::clone),
        }
    }

    /// Write `line` and a newline to a new owner-only file. `suffix` supplies
    /// the random part of the name; tests pass a fixed sequence.
    fn save(
        &self,
        tool: &str,
        line: &str,
        mut suffix: impl FnMut() -> Result<String, String>,
    ) -> Result<PathBuf, String> {
        let dir = self.directory()?;
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        let stem = file_stem(tool);
        for _ in 0..NAME_ATTEMPTS {
            let path = dir.join(format!("{sequence:04}-{stem}-{}.json", suffix()?));
            let mut file = match secure_file::create_new_owner_only(&path) {
                Ok(file) => file,
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(format!("{}: {error}", path.display())),
            };
            let written = file
                .write_all(line.as_bytes())
                .and_then(|()| file.write_all(b"\n"))
                .and_then(|()| file.flush());
            if let Err(error) = written {
                drop(file);
                // A partial file would pass for the complete result.
                let _ = std::fs::remove_file(&path);
                return Err(format!("{}: {error}", path.display()));
            }
            return Ok(path);
        }
        Err(format!(
            "{}: no unused file name after {NAME_ATTEMPTS} attempts",
            dir.display()
        ))
    }
}

/// The directory used when `--output-dir` is not given: a new
/// `mcp-repl-out-*` under the system temp directory, owner-only.
fn create_default_dir() -> Result<PathBuf, String> {
    let mut builder = tempfile::Builder::new();
    builder.prefix("mcp-repl-out-");
    // Requested at creation so the directory is never open to other
    // accounts, and set again below for a restrictive umask.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    let dir = builder
        .tempdir()
        .map(tempfile::TempDir::keep)
        .map_err(|error| format!("could not create a directory for saved results: {error}"))?;
    if dir.to_str().is_none() {
        let _ = std::fs::remove_dir(&dir);
        return Err(
            "the temp directory path is not valid UTF-8, so it cannot be named in a JSON \
             summary; pass --output-dir"
                .to_string(),
        );
    }
    // Set again after creation, so the umask does not decide the mode.
    secure_file::restrict_dir(&dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    Ok(dir)
}

/// Validate an explicit `--output-dir`, creating it owner-only if missing.
fn prepare_dir(dir: &Path) -> Result<(PathBuf, Option<String>), String> {
    let shown = dir.display().to_string();
    // Absolute, so the path in a summary works from any working directory.
    // Rebuilt from its components, which drops a trailing separator: with
    // one, `symlink_metadata` would follow a symlink to a directory.
    let dir: PathBuf = std::path::absolute(dir)
        .map_err(|error| format!("--output-dir {shown}: {error}"))?
        .components()
        .collect();
    if dir.to_str().is_none() {
        return Err(format!(
            "--output-dir {shown} is not valid UTF-8, so it cannot be named in a JSON summary"
        ));
    }
    let mut warning = None;
    match std::fs::symlink_metadata(&dir) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(format!(
                "--output-dir {shown} is a symbolic link; pass the directory it points to"
            ));
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Err(format!("--output-dir {shown} is not a directory"));
        }
        Ok(metadata) => warning = check_existing_dir(&metadata, &shown)?,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            secure_file::create_private_dir(&dir)
                .and_then(|()| secure_file::restrict_dir(&dir))
                .map_err(|error| format!("--output-dir {shown}: {error}"))?;
        }
        Err(error) => return Err(format!("--output-dir {shown}: {error}")),
    }
    // Catch a read-only directory now rather than at the first large result.
    tempfile::Builder::new()
        .prefix(".mcp-repl-probe")
        .tempfile_in(&dir)
        .map_err(|error| format!("--output-dir {shown} is not writable: {error}"))?;
    Ok((dir, warning))
}

/// An existing directory must not be replaceable by another account: it is
/// refused unless the current user owns it and only the owner can write to
/// it. Readable by others is allowed, with a warning.
#[cfg(unix)]
fn check_existing_dir(metadata: &std::fs::Metadata, shown: &str) -> Result<Option<String>, String> {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: geteuid has no preconditions and cannot fail.
    let uid = unsafe { libc::geteuid() };
    if metadata.uid() != uid {
        return Err(format!(
            "--output-dir {shown} belongs to another account; pass a directory you own"
        ));
    }
    let mode = metadata.mode() & 0o777;
    if mode & 0o022 != 0 {
        return Err(format!(
            "--output-dir {shown} is writable by other accounts, which could replace saved \
             results; pass a directory only you can write to"
        ));
    }
    Ok((mode & 0o077 != 0).then(|| {
        format!(
            "--output-dir {shown} is readable by other accounts; saved results hold \
             whatever a server returned, which can include credentials"
        )
    }))
}

#[cfg(not(unix))]
fn check_existing_dir(
    _metadata: &std::fs::Metadata,
    _shown: &str,
) -> Result<Option<String>, String> {
    Ok(None)
}

fn random_suffix() -> Result<String, String> {
    const ALPHABET: &[u8; 36] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut bytes = [0_u8; 6];
    getrandom::fill(&mut bytes)
        .map_err(|error| format!("could not generate a file name: {error}"))?;
    Ok(bytes
        .iter()
        .map(|byte| char::from(ALPHABET[usize::from(*byte) % ALPHABET.len()]))
        .collect())
}

/// The tool-name part of a saved file's name: only `[A-Za-z0-9_-]`, so a
/// tool name cannot add a path separator, a `..`, or a hidden-file dot.
fn file_stem(tool: &str) -> String {
    let stem: String = tool
        .chars()
        .take(MAX_STEM_CHARS)
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if stem.is_empty() {
        "result".to_string()
    } else {
        stem
    }
}

/// The fixed part of every summary. It is printed even when it alone is
/// larger than the budget.
fn minimum(
    tool: &str,
    is_error: bool,
    bytes: usize,
    max: usize,
    saved: Result<&str, &str>,
) -> Map<String, Value> {
    let mut summary = Map::new();
    summary.insert("mcpRepl".into(), FORMAT.into());
    summary.insert("tool".into(), short(tool).into());
    summary.insert("isError".into(), is_error.into());
    summary.insert("bytes".into(), bytes.into());
    summary.insert("maxOutput".into(), max.into());
    let (path, save_error) = match saved {
        Ok(path) => (Value::String(path.to_string()), Value::Null),
        Err(error) => (Value::Null, Value::String(error.to_string())),
    };
    summary.insert("path".into(), path);
    summary.insert("saveError".into(), save_error);
    summary
}

/// Bytes of the budget used so far by the summary being built.
struct Room {
    used: usize,
    max: usize,
}

impl Room {
    /// Account for `cost` more bytes if they fit, and say whether they did.
    fn take(&mut self, cost: usize) -> bool {
        if self.used + cost > self.max {
            return false;
        }
        self.used += cost;
        true
    }

    fn left(&self) -> usize {
        self.max.saturating_sub(self.used)
    }
}

fn json_len(value: &Value) -> usize {
    value.to_string().len()
}

/// What adding `"key":value` to a non-empty object costs. Keys here are
/// fixed ASCII names, which JSON does not escape.
fn member_cost(key: &str, value: &Value) -> usize {
    1 + key.len() + 2 + 1 + json_len(value)
}

/// What appending `value` to an array of `len` elements costs.
fn element_cost(len: usize, value: &Value) -> usize {
    usize::from(len > 0) + json_len(value)
}

/// Where a text preview goes in the summary.
enum Slot {
    Block(usize),
    Value,
}

/// Build the summary that replaces an over-budget result.
///
/// The fixed fields come first. Then, while they fit, in this order: the
/// `content` list and `blocksOmitted` (or `value` for a [`Shape::Value`]),
/// the `structuredContent` and `meta` descriptors, block descriptors in
/// order, object keys, and text previews. Block descriptors stop at the
/// first that does not fit, so entry `i` always describes block `i`, and
/// `blocksOmitted` counts the rest.
pub(crate) fn summarize(
    tool: &str,
    is_error: bool,
    shape: Shape,
    value: &Value,
    bytes: usize,
    max: usize,
    saved: Result<&str, &str>,
) -> Value {
    let mut summary = minimum(tool, is_error, bytes, max, saved);
    let mut room = Room {
        used: serde_json::to_string(&summary).map_or(0, |line| line.len()),
        max,
    };
    let mut previews: Vec<(Slot, &str)> = Vec::new();
    match shape {
        Shape::Result => summarize_result(&mut summary, &mut room, value, &mut previews),
        Shape::Value => {
            let described = describe(value);
            if room.take(member_cost("value", &described)) {
                summary.insert("value".into(), described);
                match value {
                    Value::String(text) => previews.push((Slot::Value, text)),
                    Value::Object(map) => add_keys(&mut summary["value"], map, &mut room),
                    _ => {}
                }
            }
        }
    }
    let mut summary = Value::Object(summary);
    add_previews(&mut summary, &previews, &mut room);
    summary
}

fn summarize_result<'a>(
    summary: &mut Map<String, Value>,
    room: &mut Room,
    value: &'a Value,
    previews: &mut Vec<(Slot, &'a str)>,
) {
    let object = value.as_object();
    let content: &[Value] = object
        .and_then(|object| object.get("content"))
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice);
    let total = content.len();
    let listed = room.take(
        member_cost("content", &Value::Array(Vec::new()))
            + member_cost("blocksOmitted", &total.into()),
    );
    if listed {
        summary.insert("content".into(), Value::Array(Vec::new()));
        summary.insert("blocksOmitted".into(), total.into());
    }

    let mut keyed = Vec::new();
    for (field, name) in [
        ("structuredContent", "structuredContent"),
        ("_meta", "meta"),
    ] {
        let Some(field_value) = object.and_then(|object| object.get(field)) else {
            continue;
        };
        let described = describe(field_value);
        if room.take(member_cost(name, &described)) {
            summary.insert(name.into(), described);
            if let Value::Object(map) = field_value {
                keyed.push((name, map));
            }
        }
    }

    if listed {
        let mut entries = Vec::new();
        for block in content.iter().take(MAX_BLOCKS) {
            let (entry, text) = block_summary(block);
            if !room.take(element_cost(entries.len(), &entry)) {
                break;
            }
            if let Some(text) = text {
                previews.push((Slot::Block(entries.len()), text));
            }
            entries.push(entry);
        }
        // Counting down never adds digits, so the space taken for `total`
        // covers the final count; give back the difference.
        let omitted = total - entries.len();
        room.used -= total.to_string().len() - omitted.to_string().len();
        summary.insert("content".into(), Value::Array(entries));
        summary.insert("blocksOmitted".into(), omitted.into());
    }

    for (name, map) in keyed {
        add_keys(&mut summary[name], map, room);
    }
}

/// Fill a descriptor's `keys` list while the keys fit. `keyCount` already
/// says how many there are.
fn add_keys(descriptor: &mut Value, map: &Map<String, Value>, room: &mut Room) {
    let Some(keys) = descriptor.get_mut("keys").and_then(Value::as_array_mut) else {
        return;
    };
    for key in map
        .keys()
        .filter(|key| key.len() <= MAX_FIELD_BYTES)
        .take(MAX_KEYS)
    {
        let key = Value::String(key.clone());
        if !room.take(element_cost(keys.len(), &key)) {
            break;
        }
        keys.push(key);
    }
}

/// Fill text previews, in block order, from whatever the budget has left.
fn add_previews(summary: &mut Value, previews: &[(Slot, &str)], room: &mut Room) {
    for (slot, text) in previews {
        if text.is_empty() {
            continue;
        }
        let Some(space) = room.left().checked_sub(PREVIEW_OVERHEAD) else {
            break;
        };
        let (preview, used) = escaped_prefix(text, space);
        if preview.is_empty() {
            break;
        }
        let target = match slot {
            Slot::Block(index) => &mut summary["content"][*index],
            Slot::Value => &mut summary["value"],
        };
        target["preview"] = Value::String(preview.to_string());
        room.used += PREVIEW_OVERHEAD + used;
    }
}

/// Bytes `c` takes inside a JSON string as serde_json writes it.
fn escaped_len(c: char) -> usize {
    match c {
        '"' | '\\' | '\u{08}' | '\u{0c}' | '\n' | '\r' | '\t' => 2,
        c if u32::from(c) < 0x20 => 6,
        c => c.len_utf8(),
    }
}

/// The longest prefix of `text`, ending on a character boundary, whose JSON
/// escaping fits in `room` bytes, and the bytes that escaping takes.
fn escaped_prefix(text: &str, room: usize) -> (&str, usize) {
    let mut used = 0;
    for (index, c) in text.char_indices() {
        let cost = escaped_len(c);
        if used + cost > room {
            return (&text[..index], used);
        }
        used += cost;
    }
    (text, used)
}

/// One content block's entry, and its text when it has any to preview.
fn block_summary(block: &Value) -> (Value, Option<&str>) {
    let kind = block
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let mut entry = Map::new();
    entry.insert("type".into(), short(kind).into());
    let mut text = None;
    match kind {
        "text" => {
            let body = str_field(block, "text").unwrap_or("");
            entry.insert("textBytes".into(), body.len().into());
            text = Some(body);
        }
        "image" | "audio" => {
            copy_short(&mut entry, block, "mimeType");
            let data = str_field(block, "data").map_or(0, str::len);
            entry.insert("base64Chars".into(), data.into());
        }
        "resource" => {
            let resource = block.get("resource").unwrap_or(&Value::Null);
            copy_short(&mut entry, resource, "uri");
            copy_short(&mut entry, resource, "mimeType");
            if let Some(body) = str_field(resource, "text") {
                entry.insert("textBytes".into(), body.len().into());
                text = Some(body);
            }
            if let Some(blob) = str_field(resource, "blob") {
                entry.insert("base64Chars".into(), blob.len().into());
            }
        }
        "resource_link" => {
            for key in ["uri", "name", "mimeType"] {
                copy_short(&mut entry, block, key);
            }
        }
        _ => {
            entry.insert("bytes".into(), block.to_string().len().into());
        }
    }
    (Value::Object(entry), text)
}

/// Type, serialized size, and shape of a value, without its contents. An
/// object's `keys` start empty; [`add_keys`] fills them while they fit.
fn describe(value: &Value) -> Value {
    let kind = match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    };
    let mut out = Map::new();
    out.insert("type".into(), kind.into());
    out.insert("bytes".into(), value.to_string().len().into());
    match value {
        Value::Array(items) => {
            out.insert("length".into(), items.len().into());
        }
        Value::Object(map) => {
            out.insert("keyCount".into(), map.len().into());
            out.insert("keys".into(), Value::Array(Vec::new()));
        }
        _ => {}
    }
    Value::Object(out)
}

fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn copy_short(entry: &mut Map<String, Value>, from: &Value, key: &str) {
    if let Some(text) = str_field(from, key) {
        entry.insert(key.to_string(), short(text).into());
    }
}

/// `text`, or its first bytes plus `...` when longer than a summary field
/// should be.
fn short(text: &str) -> String {
    if text.len() <= MAX_FIELD_BYTES {
        return text.to_string();
    }
    let mut end = MAX_FIELD_BYTES - 3;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::property::{GENERATED_CASES, Generator};
    use serde_json::json;

    const SAVED: Result<&str, &str> = Ok("/saved/result.json");

    fn budget_in(dir: &Path, max: usize) -> Budget {
        Budget::new(max, Some(dir)).unwrap().0
    }

    fn text_result(text: &str) -> Value {
        json!({"content": [{"type": "text", "text": text}]})
    }

    fn files_in(dir: &Path) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        files.sort();
        files
    }

    fn floor(tool: &str, is_error: bool, bytes: usize, max: usize) -> usize {
        serde_json::to_string(&minimum(tool, is_error, bytes, max, SAVED))
            .unwrap()
            .len()
    }

    #[test]
    fn under_budget_value_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let budget = budget_in(dir.path(), 1024);
        let value = text_result("hi");
        assert!(budget.apply("echo", false, Shape::Result, &value).is_none());
        assert!(files_in(dir.path()).is_empty(), "nothing is saved");
    }

    #[test]
    fn exact_budget_is_not_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let value = text_result("exactly at the limit");
        let size = value.to_string().len();
        let at = budget_in(dir.path(), size);
        assert!(at.apply("t", false, Shape::Result, &value).is_none());
        let under = budget_in(dir.path(), size - 1);
        assert!(under.apply("t", false, Shape::Result, &value).is_some());
    }

    #[test]
    fn summary_reports_version_bytes_and_path() {
        let dir = tempfile::tempdir().unwrap();
        let value = text_result(&"x".repeat(5000));
        let line = value.to_string();
        let truncation = budget_in(dir.path(), 600)
            .apply("scan", false, Shape::Result, &value)
            .unwrap();
        let summary = &truncation.summary;
        assert!(truncation.save_error.is_none());
        assert_eq!(summary["mcpRepl"], FORMAT);
        assert_eq!(summary["tool"], "scan");
        assert_eq!(summary["isError"], false);
        assert_eq!(summary["bytes"], line.len());
        assert_eq!(summary["maxOutput"], 600);
        assert_eq!(summary["saveError"], Value::Null);
        assert_eq!(summary["content"][0]["type"], "text");
        assert_eq!(summary["content"][0]["textBytes"], 5000);
        assert_eq!(summary["blocksOmitted"], 0);
        assert!(summary["content"][0]["preview"].as_str().unwrap().len() > 100);
        assert!(summary.to_string().len() <= 600);
        let path = PathBuf::from(summary["path"].as_str().unwrap());
        assert!(path.starts_with(std::path::absolute(dir.path()).unwrap()));
        let name = path.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("0001-scan-") && name.ends_with(".json"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), format!("{line}\n"));
    }

    #[test]
    fn text_preview_stops_on_a_char_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let text = "é\"\n中🦀\u{1}".repeat(200);
        let value = text_result(&text);
        for max in 600..640 {
            let summary = budget_in(dir.path(), max)
                .apply("t", false, Shape::Result, &value)
                .unwrap()
                .summary;
            let preview = summary["content"][0]["preview"].as_str().unwrap_or("");
            assert!(text.starts_with(preview));
            assert!(summary.to_string().len() <= max, "over budget at {max}");
        }
    }

    #[test]
    fn structured_content_is_described_not_cut() {
        let items: Vec<Value> = (0..200).map(|id| json!({"id": id})).collect();
        let structured = json!({"items": items, "total": 200});
        let value = json!({
            "content": [{"type": "text", "text": "200 items"}],
            "structuredContent": structured,
            "_meta": {"fixture/extra": "kept"},
        });
        let summary = summarize("t", false, Shape::Result, &value, 9999, 600, SAVED);
        let described = &summary["structuredContent"];
        assert_eq!(described["type"], "object");
        assert_eq!(described["keys"], json!(["items", "total"]));
        assert_eq!(described["keyCount"], 2);
        assert_eq!(described["bytes"], structured.to_string().len());
        assert!(described.get("preview").is_none());
        assert_eq!(summary["meta"]["keys"], json!(["fixture/extra"]));
    }

    #[test]
    fn base64_blocks_report_length_only() {
        let value = json!({"content": [
            {"type": "image", "mimeType": "image/png", "data": "A".repeat(4000)},
            {"type": "audio", "mimeType": "audio/wav", "data": "AAAA"},
            {"type": "resource", "resource": {"uri": "r://b", "blob": "QUJD"}},
            {"type": "resource_link", "uri": "r://l", "name": "link"},
        ]});
        let summary = summarize("t", false, Shape::Result, &value, 4200, 2000, SAVED);
        let blocks = summary["content"].as_array().unwrap();
        assert_eq!(
            blocks[0],
            json!({"type": "image", "mimeType": "image/png", "base64Chars": 4000})
        );
        assert_eq!(blocks[1]["base64Chars"], 4);
        assert_eq!(
            blocks[2],
            json!({"type": "resource", "uri": "r://b", "base64Chars": 4})
        );
        assert_eq!(
            blocks[3],
            json!({"type": "resource_link", "uri": "r://l", "name": "link"})
        );
        assert!(!summary.to_string().contains("AAAA"));
    }

    #[test]
    fn tool_error_stays_an_error_in_the_summary() {
        let dir = tempfile::tempdir().unwrap();
        let value =
            json!({"content": [{"type": "text", "text": "boom ".repeat(100)}], "isError": true});
        let summary = budget_in(dir.path(), 200)
            .apply("fail", true, Shape::Result, &value)
            .unwrap()
            .summary;
        assert_eq!(summary["isError"], true);
        let files = files_in(dir.path());
        assert_eq!(files.len(), 1);
        let saved = std::fs::read_to_string(&files[0]).unwrap();
        assert!(saved.contains(r#""isError":true"#));
    }

    #[test]
    fn empty_content_summary_is_well_formed() {
        let value = json!({"content": []});
        let summary = summarize("t", false, Shape::Result, &value, 9999, 400, SAVED);
        assert_eq!(summary["content"], json!([]));
        assert_eq!(summary["blocksOmitted"], 0);
        assert!(summary.get("structuredContent").is_none());
    }

    #[test]
    fn below_the_floor_the_summary_is_exactly_the_minimum() {
        let value = text_result(&"x".repeat(5000));
        for max in [1, 50, floor("t", true, 9999, 50) - 1] {
            let summary = summarize("t", true, Shape::Result, &value, 9999, max, SAVED);
            assert_eq!(
                summary,
                Value::Object(minimum("t", true, 9999, max, SAVED)),
                "at {max}"
            );
        }
    }

    #[test]
    fn block_list_is_capped_with_an_omitted_count() {
        let blocks: Vec<Value> = (0..40)
            .map(|n| json!({"type": "text", "text": format!("block {n}")}))
            .collect();
        let value = json!({"content": blocks});
        let summary = summarize("t", false, Shape::Result, &value, 9999, 100_000, SAVED);
        assert_eq!(summary["content"].as_array().unwrap().len(), MAX_BLOCKS);
        assert_eq!(summary["blocksOmitted"], 40 - MAX_BLOCKS);
    }

    /// Many descriptors used to be added regardless of the budget: 32
    /// resource links made a summary of several kilobytes at a 2048 limit.
    #[test]
    fn descriptors_are_trimmed_to_the_budget_and_counted() {
        let blocks: Vec<Value> = (0..40)
            .map(|n| {
                json!({"type": "resource_link", "name": format!("link {n}"),
                       "uri": format!("res://{n}/{}", "p".repeat(240))})
            })
            .collect();
        let value = json!({"content": blocks});
        let summary = summarize("t", false, Shape::Result, &value, 99_999, 2048, SAVED);
        let described = summary["content"].as_array().unwrap().len();
        assert!(
            summary.to_string().len() <= 2048,
            "{}",
            summary.to_string().len()
        );
        assert!(described > 0 && described < MAX_BLOCKS, "{described}");
        assert_eq!(summary["blocksOmitted"], 40 - described);
        assert_eq!(
            summary["content"][0]["name"], "link 0",
            "a prefix, in order"
        );
    }

    #[test]
    fn keys_are_trimmed_to_the_budget() {
        let structured: Map<String, Value> = (0..32)
            .map(|n| (format!("{n:02}{}", "k".repeat(200)), json!(n)))
            .collect();
        let value = json!({"content": [], "structuredContent": structured});
        let summary = summarize("t", false, Shape::Result, &value, 99_999, 1500, SAVED);
        let described = &summary["structuredContent"];
        assert_eq!(described["keyCount"], 32);
        let keys = described["keys"].as_array().unwrap().len();
        assert!(keys > 0 && keys < 32, "{keys}");
        assert!(summary.to_string().len() <= 1500);
    }

    #[test]
    fn a_selected_value_is_described_under_value() {
        let value = json!("selected text ".repeat(50));
        let summary = summarize("t", false, Shape::Value, &value, 700, 400, SAVED);
        assert_eq!(summary["value"]["type"], "string");
        let preview = summary["value"]["preview"].as_str().unwrap();
        assert!(preview.starts_with("selected text"));
        assert!(summary.get("content").is_none());
    }

    #[test]
    fn a_selected_object_with_a_content_array_is_still_a_value() {
        let value = json!({"content": [{"type": "text", "text": "business data"}], "id": 7});
        let summary = summarize("t", false, Shape::Value, &value, 9999, 1000, SAVED);
        assert_eq!(summary["value"]["type"], "object");
        assert_eq!(summary["value"]["keys"], json!(["content", "id"]));
        assert!(summary.get("content").is_none());
    }

    #[test]
    fn long_server_strings_are_shortened_in_a_summary() {
        let long = "u".repeat(5000);
        let value = json!({"content": [{"type": "resource_link", "uri": long, "name": "n"}]});
        let summary = summarize("t", false, Shape::Result, &value, 9999, 10_000, SAVED);
        let uri = summary["content"][0]["uri"].as_str().unwrap();
        assert_eq!(uri.len(), MAX_FIELD_BYTES);
        assert!(uri.ends_with("..."));
    }

    /// What the protocol type keeps is what the saved file can hold: the
    /// result's `_meta` and a block's annotations and `_meta` survive.
    #[test]
    fn meta_and_content_fields_survive_the_protocol_round_trip() {
        let wire = json!({
            "content": [{
                "type": "text",
                "text": "hi",
                "annotations": {"priority": 0.5},
                "_meta": {"block/extra": 1},
            }],
            "isError": true,
            "structuredContent": {"a": 1},
            "_meta": {"fixture/extra": "kept"},
        });
        let result: tower_mcp::CallToolResult = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(&result).unwrap(), wire);
    }

    #[test]
    fn tool_names_are_sanitized_in_file_names() {
        assert_eq!(file_stem("../etc/passwd"), "___etc_passwd");
        assert_eq!(file_stem(".hidden"), "_hidden");
        assert_eq!(file_stem("get-crate_info2"), "get-crate_info2");
        assert_eq!(file_stem(""), "result");
        assert_eq!(file_stem(&"n".repeat(500)).len(), MAX_STEM_CHARS);
    }

    #[test]
    fn existing_name_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let budget = budget_in(dir.path(), 1);
        let taken = dir.path().join("0001-t-aaaaaa.json");
        std::fs::write(&taken, "earlier").unwrap();
        let mut names = ["aaaaaa", "bbbbbb"].into_iter();
        let saved = budget
            .save("t", "{}", || Ok(names.next().unwrap().to_string()))
            .unwrap();
        assert_eq!(saved.file_name().unwrap(), "0001-t-bbbbbb.json");
        assert_eq!(std::fs::read_to_string(&taken).unwrap(), "earlier");
        assert_eq!(std::fs::read_to_string(&saved).unwrap(), "{}\n");
    }

    #[test]
    fn an_output_dir_that_is_a_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("plain");
        std::fs::write(&file, "").unwrap();
        let error = Budget::new(10, Some(&file)).err().unwrap();
        assert!(error.contains("not a directory"), "{error}");
    }

    /// Deterministic save failure: the default directory could not be made.
    #[test]
    fn an_unavailable_directory_yields_save_error_and_keeps_is_error() {
        let cell = OnceLock::new();
        cell.set(Err("no temp directory".to_string())).unwrap();
        let budget = Budget {
            max: 300,
            destination: Destination::Default(cell),
            sequence: AtomicU64::new(0),
        };
        let value = json!({"content": [{"type": "text", "text": "z".repeat(500)}],
                           "isError": true});
        let truncation = budget.apply("fail", true, Shape::Result, &value).unwrap();
        assert_eq!(truncation.save_error.as_deref(), Some("no temp directory"));
        assert_eq!(truncation.summary["path"], Value::Null);
        assert_eq!(truncation.summary["saveError"], "no temp directory");
        assert_eq!(truncation.summary["isError"], true);
        assert!(truncation.summary.to_string().len() <= 300);
    }

    #[test]
    fn property_summaries_are_valid_json_within_budget_and_keep_is_error() {
        let mut generator = Generator::new(0x06);
        for _ in 0..GENERATED_CASES {
            let is_error = generator.index(2) == 1;
            let mut content = Vec::new();
            for _ in 0..generator.index(40) {
                content.push(match generator.index(4) {
                    0 => json!({"type": "text", "text": generator.text(400)}),
                    1 => json!({"type": "image", "mimeType": "image/png",
                                "data": "A".repeat(generator.index(400))}),
                    2 => json!({"type": "resource_link", "name": generator.text(30),
                                "uri": generator.text(300)}),
                    _ => json!({"type": "resource", "resource":
                                {"uri": generator.text(20), "text": generator.text(200)}}),
                });
            }
            let mut value = json!({"content": content, "isError": is_error});
            if generator.index(2) == 1 {
                let structured: Map<String, Value> = (0..generator.index(40))
                    .map(|_| (generator.text(60), json!(generator.text(20))))
                    .collect();
                value["structuredContent"] = Value::Object(structured);
            }
            let shape = if generator.index(4) == 0 {
                Shape::Value
            } else {
                Shape::Result
            };
            let bytes = value.to_string().len();
            let max = 1 + generator.index(3000);
            let summary = summarize("tool", is_error, shape, &value, bytes, max, SAVED);
            let line = summary.to_string();
            let reparsed: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(reparsed["isError"], is_error);
            assert_eq!(reparsed["mcpRepl"], FORMAT);
            let floor = floor("tool", is_error, bytes, max);
            if max >= floor {
                assert!(line.len() <= max, "{} > {max}: {line}", line.len());
            } else {
                assert_eq!(
                    reparsed,
                    Value::Object(minimum("tool", is_error, bytes, max, SAVED))
                );
            }
            if shape == Shape::Value {
                assert!(reparsed.get("content").is_none());
                continue;
            }
            let described = reparsed["content"].as_array().map_or(0, Vec::len);
            if let Some(omitted) = reparsed["blocksOmitted"].as_u64() {
                assert_eq!(omitted as usize + described, content_len(&value));
            }
            for (index, block) in value["content"].as_array().unwrap().iter().enumerate() {
                if let Some(preview) = reparsed["content"][index]["preview"].as_str() {
                    let text = block["text"]
                        .as_str()
                        .or_else(|| block["resource"]["text"].as_str())
                        .unwrap();
                    assert!(text.starts_with(preview));
                }
            }
        }
    }

    fn content_len(value: &Value) -> usize {
        value["content"].as_array().map_or(0, Vec::len)
    }

    #[cfg(unix)]
    mod unix {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        fn mode_of(path: &Path) -> u32 {
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777
        }

        fn chmod(path: &Path, mode: u32) {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        }

        #[test]
        fn artifact_is_owner_only_and_matches_the_json_line() {
            let dir = tempfile::tempdir().unwrap();
            let value = text_result(&"y".repeat(3000));
            let summary = budget_in(dir.path(), 100)
                .apply("t", false, Shape::Result, &value)
                .unwrap()
                .summary;
            let path = Path::new(summary["path"].as_str().unwrap());
            assert_eq!(mode_of(path), 0o600);
            assert_eq!(std::fs::read_to_string(path).unwrap(), format!("{value}\n"));
        }

        #[test]
        fn output_dir_is_created_owner_only() {
            let dir = tempfile::tempdir().unwrap();
            let nested = dir.path().join("a").join("results");
            let (_, warning) = Budget::new(10, Some(&nested)).unwrap();
            assert!(warning.is_none());
            assert_eq!(mode_of(&nested), 0o700);
            assert!(files_in(&nested).is_empty(), "the probe file is removed");
        }

        #[test]
        fn a_readable_output_dir_is_accepted_with_a_warning() {
            let dir = tempfile::tempdir().unwrap();
            chmod(dir.path(), 0o755);
            let (_, warning) = Budget::new(10, Some(dir.path())).unwrap();
            assert!(warning.unwrap().contains("credentials"));
        }

        #[test]
        fn an_output_dir_others_can_write_is_refused() {
            for mode in [0o770, 0o707, 0o777] {
                let dir = tempfile::tempdir().unwrap();
                chmod(dir.path(), mode);
                let error = Budget::new(10, Some(dir.path())).err().unwrap();
                assert!(error.contains("writable by other accounts"), "{error}");
                chmod(dir.path(), 0o700);
            }
        }

        #[test]
        fn symlinked_output_dir_is_refused_with_or_without_a_trailing_slash() {
            let dir = tempfile::tempdir().unwrap();
            let real = dir.path().join("real");
            std::fs::create_dir(&real).unwrap();
            let link = dir.path().join("link");
            std::os::unix::fs::symlink(&real, &link).unwrap();
            let trailing = PathBuf::from(format!("{}/", link.display()));
            for path in [&link, &trailing] {
                let error = Budget::new(10, Some(path)).err().unwrap();
                assert!(
                    error.contains("symbolic link"),
                    "{}: {error}",
                    path.display()
                );
            }
        }

        #[test]
        fn a_non_utf8_output_dir_is_refused() {
            use std::os::unix::ffi::OsStrExt;
            let dir = tempfile::tempdir().unwrap();
            let name = std::ffi::OsStr::from_bytes(b"results-\xff");
            let error = Budget::new(10, Some(&dir.path().join(name))).err().unwrap();
            assert!(error.contains("not valid UTF-8"), "{error}");
        }

        /// Root can write anywhere, so on a root runner this checks nothing;
        /// the deterministic save failure is tested above and end to end.
        #[test]
        fn unwritable_output_dir_is_refused_at_startup() {
            let dir = tempfile::tempdir().unwrap();
            let locked = dir.path().join("locked");
            std::fs::create_dir(&locked).unwrap();
            chmod(&locked, 0o500);
            if std::fs::File::create(locked.join("probe")).is_err() {
                let error = Budget::new(10, Some(&locked)).err().unwrap();
                assert!(error.contains("not writable"), "{error}");
            }
            chmod(&locked, 0o700);
        }

        #[test]
        fn default_directory_is_private_and_created_on_first_use() {
            let (budget, _) = Budget::new(10, None).unwrap();
            let Destination::Default(cell) = &budget.destination else {
                panic!("no --output-dir means the default destination");
            };
            assert!(cell.get().is_none(), "nothing is created up front");
            let saved = budget.save("t", "{}", random_suffix).unwrap();
            let dir = saved.parent().unwrap().to_path_buf();
            assert_eq!(mode_of(&dir), 0o700);
            assert_eq!(mode_of(&saved), 0o600);
            let name = dir.file_name().unwrap().to_str().unwrap();
            assert!(name.starts_with("mcp-repl-out-"));
            std::fs::remove_dir_all(dir).unwrap();
        }
    }
}
