//! Receipts and verification for the manager's source and installed copies.
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{ErrorKind, Read};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{Provider, Result};
use crate::secure_file;

pub(super) type Files = BTreeMap<String, String>;
const MAX_FILE: u64 = 1024 * 1024;
const PLUGIN_ROOT: &str = "plugins/mcp-repl/";

const PORTABLE: &str = include_str!("../plugin_assets/plugin.json");
const CLAUDE_CATALOG: &str = include_str!("../plugin_assets/claude-marketplace.json");
const CLAUDE_MANIFEST: &str = include_str!("../plugin_assets/claude-plugin.json");
const CODEX_CATALOG: &str = include_str!("../plugin_assets/codex-marketplace.json");
const SKILL: &str = include_str!("../plugin_assets/skills/mcp-repl/SKILL.md");
const SESSION_DRIVER: &str =
    include_str!("../plugin_assets/skills/mcp-repl/scripts/session_driver.py");

pub(super) fn bundle_version() -> String {
    // Stable FNV-1a cache key. Ownership checks compare the actual file bytes.
    let mut fingerprint = 0xcbf29ce484222325u64;
    for asset in [
        PORTABLE,
        CLAUDE_CATALOG,
        CLAUDE_MANIFEST,
        CODEX_CATALOG,
        SKILL,
        SESSION_DRIVER,
    ] {
        for byte in asset.bytes().chain(std::iter::once(0)) {
            fingerprint = (fingerprint ^ u64::from(byte)).wrapping_mul(0x100000001b3);
        }
    }
    let release = env!("CARGO_PKG_VERSION");
    let separator = if release.contains('+') { "." } else { "+" };
    format!("{release}{separator}bundle.{fingerprint:016x}")
}

pub(super) fn bundle(provider: Provider, version: &str) -> Files {
    let (marketplace, catalog) = match provider {
        Provider::Claude => (".claude-plugin/marketplace.json", CLAUDE_CATALOG),
        Provider::Codex => (".agents/plugins/marketplace.json", CODEX_CATALOG),
    };
    [
        (marketplace, catalog),
        (
            "plugins/mcp-repl/.claude-plugin/plugin.json",
            CLAUDE_MANIFEST,
        ),
        ("plugins/mcp-repl/plugin.json", PORTABLE),
        ("plugins/mcp-repl/skills/mcp-repl/SKILL.md", SKILL),
    ]
    .into_iter()
    .map(|(path, contents)| (path.to_owned(), contents.replace("@VERSION@", version)))
    .chain(std::iter::once((
        "plugins/mcp-repl/skills/mcp-repl/scripts/session_driver.py".into(),
        SESSION_DRIVER.replace(
            "@SESSION_DRIVER_MIN_REPL_VERSION@",
            env!("CARGO_PKG_VERSION"),
        ),
    )))
    .collect()
}

pub(super) fn plugin_files(files: &Files) -> Files {
    files
        .iter()
        .filter_map(|(path, contents)| {
            Some((path.strip_prefix(PLUGIN_ROOT)?.to_owned(), contents.clone()))
        })
        .collect()
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Receipt {
    pub(super) schema: u32,
    pub(super) profile: PathBuf,
    pub(super) files: Files,
    pub(super) version: String,
    pub(super) caches: BTreeMap<String, Files>,
    pub(super) pending: Option<String>,
}

impl Receipt {
    pub(super) fn active(&self) -> bool {
        !self.version.is_empty()
    }
}

pub(super) struct State {
    pub(super) source: PathBuf,
    receipt: PathBuf,
    lock: PathBuf,
}

impl State {
    pub(super) fn new(base: &Path, provider: Provider) -> Self {
        let name = provider.program();
        Self {
            source: base.join(name),
            receipt: base.join(format!("{name}.json")),
            lock: base.join(format!("{name}.lock")),
        }
    }

    pub(super) fn lock(&self, create: bool) -> Result<Option<File>> {
        if !create && metadata(&self.lock)?.is_none() {
            return Ok(None);
        }
        let metadata = metadata(&self.lock)?;
        if metadata.is_some_and(|metadata| !metadata.is_file()) {
            return Err(format!("invalid lock file {}", self.lock.display()));
        }
        if create {
            fs_result(&self.lock, secure_file::create_parent_dir(&self.lock))?;
        }
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(create)
            .create(create)
            .truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let file = fs_result(&self.lock, options.open(&self.lock))?;
        let result = if create {
            file.try_lock()
        } else {
            file.try_lock_shared()
        };
        result.map_err(|error| format!("cannot lock {}: {error}", self.lock.display()))?;
        Ok(Some(file))
    }

    pub(super) fn read(&self, profile: &Path) -> Result<Option<Receipt>> {
        if metadata(&self.receipt)?.is_none() {
            if metadata(&self.source)?.is_some() {
                return Err(format!(
                    "foreign source directory {}",
                    self.source.display()
                ));
            }
            return Ok(None);
        }
        let receipt: Receipt = serde_json::from_str(&read_file(&self.receipt)?)
            .map_err(|_| format!("invalid receipt {}", self.receipt.display()))?;
        if receipt.schema != 1 || receipt.profile != profile {
            return Err(format!(
                "receipt {} belongs to another profile or schema",
                self.receipt.display()
            ));
        }
        if !receipt.caches.keys().all(|version| {
            !version.is_empty()
                && version
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+'))
                && version.as_bytes()[0].is_ascii_digit()
        }) || (receipt.active() && !receipt.caches.contains_key(&receipt.version))
        {
            return Err(format!(
                "invalid cache versions in {}",
                self.receipt.display()
            ));
        }
        if let Some(operation) = receipt.pending.as_deref() {
            if !matches!(operation, "install" | "update" | "uninstall") {
                return Err(format!(
                    "invalid pending operation in {}",
                    self.receipt.display()
                ));
            }
            // A pending operation can legitimately leave an incomplete or
            // changed tree. Its receipt is the only evidence read here.
            return Ok(Some(receipt));
        }
        if receipt.active() {
            verify_tree(&self.source, &receipt.files)?;
        } else if !receipt.files.is_empty() || metadata(&self.source)?.is_some() {
            return Err(format!(
                "unexpected source for inactive receipt {}",
                self.receipt.display()
            ));
        }
        Ok(Some(receipt))
    }

    pub(super) fn pending_message(&self, operation: &str, provider: Provider) -> String {
        format!(
            "previous operation ({operation}) is unfinished or uncertain. Inspect receipt {}. {}",
            self.receipt.display(),
            provider.inspection_hint(),
        )
    }

    pub(super) fn discard_receipt(&self) -> Result<()> {
        metadata(&self.receipt)?;
        fs_result(&self.receipt, fs::remove_file(&self.receipt))
    }

    pub(super) fn save(&self, receipt: &Receipt) -> Result<()> {
        let data = serde_json::to_string_pretty(receipt).map_err(|error| error.to_string())?;
        if data.len() as u64 > MAX_FILE {
            return Err("plugin receipt exceeds 1 MiB".into());
        }
        metadata(&self.receipt)?;
        fs_result(
            &self.receipt,
            secure_file::write_atomic(&self.receipt, &data),
        )
    }

    pub(super) fn install_source(&self, pending: &Receipt, files: &Files) -> Result<()> {
        let staging = self.prepare_source(files)?;
        self.save(pending)?;
        self.publish_source(staging.path())
    }

    fn prepare_source(&self, files: &Files) -> Result<tempfile::TempDir> {
        let parent = self.source.parent().expect("source has a parent");
        let staging = fs_result(parent, tempfile::tempdir_in(parent))?;
        write_files(staging.path(), files)?;
        self.require_absent_source()?;
        Ok(staging)
    }

    fn require_absent_source(&self) -> Result<()> {
        if metadata(&self.source)?.is_some() {
            return Err(format!(
                "foreign source directory {}",
                self.source.display()
            ));
        }
        Ok(())
    }

    fn publish_source(&self, staging: &Path) -> Result<()> {
        self.require_absent_source()?;
        fs_result(&self.source, fs::rename(staging, &self.source))
    }

    #[cfg(test)]
    fn create_source(&self, files: &Files) -> Result<()> {
        let staging = self.prepare_source(files)?;
        self.publish_source(staging.path())
    }

    pub(super) fn replace_source(&self, before: &Files, after: &Files) -> Result<()> {
        verify_tree(&self.source, before)?;
        // Keep the catalog registered at a stable path. Publish each file
        // atomically and keep the pending receipt during the whole operation.
        write_files(&self.source, after)?;
        for name in before.keys().filter(|name| !after.contains_key(*name)) {
            let path = self.source.join(name);
            metadata(&path)?;
            fs_result(&path, fs::remove_file(&path))?;
        }
        let after_dirs = directories(after)?;
        let mut obsolete = directories(before)?;
        obsolete.retain(|dir| !after_dirs.contains(dir));
        for dir in obsolete.iter().rev() {
            let path = self.source.join(dir);
            metadata(&path)?;
            fs_result(&path, fs::remove_dir(&path))?;
        }
        verify_tree(&self.source, after)
    }

    pub(super) fn remove(&self, files: &Files) -> Result<()> {
        verify_tree(&self.source, files)?;
        for name in files.keys() {
            let path = self.source.join(name);
            metadata(&path)?;
            fs_result(&path, fs::remove_file(&path))?;
        }
        for dir in directories(files)?.iter().rev() {
            let path = self.source.join(dir);
            metadata(&path)?;
            fs_result(&path, fs::remove_dir(&path))?;
        }
        metadata(&self.source)?;
        fs_result(&self.source, fs::remove_dir(&self.source))
    }
}

fn directories(files: &Files) -> Result<std::collections::BTreeSet<PathBuf>> {
    let mut dirs = std::collections::BTreeSet::new();
    if files.is_empty() {
        return Err("empty bundle receipt".into());
    }
    for name in files.keys() {
        let path = Path::new(name);
        if name.is_empty()
            || path
                .components()
                .any(|part| !matches!(part, Component::Normal(_)))
        {
            return Err("invalid bundle receipt path".into());
        }
        dirs.extend(
            path.ancestors()
                .skip(1)
                .filter(|p| !p.as_os_str().is_empty())
                .map(Path::to_owned),
        );
    }
    Ok(dirs)
}

pub(super) fn verify_tree(root: &Path, expected: &Files) -> Result<()> {
    verify_files(root, expected, false)
}

pub(super) fn verify_cache_version(
    provider: Provider,
    root: &Path,
    expected: &Files,
) -> Result<()> {
    verify_files(root, expected, provider == Provider::Claude)
}

fn verify_files(root: &Path, expected: &Files, claude_cache: bool) -> Result<()> {
    let dirs = directories(expected)?;
    for dir in std::iter::once(Path::new("")).chain(dirs.iter().map(PathBuf::as_path)) {
        let path = root.join(dir);
        if !metadata(&path)?.is_some_and(|metadata| metadata.is_dir()) {
            return Err(format!("expected managed directory {}", path.display()));
        }
        for entry in fs_result(&path, fs::read_dir(&path))? {
            let entry = fs_result(&path, entry)?;
            let relative = dir.join(entry.file_name());
            // Claude writes this marker to retained cache versions on update
            // and uninstall. It is provider metadata, outside the bundle.
            if claude_cache && relative == Path::new(".orphaned_at") {
                let path = entry.path();
                if !metadata(&path)?.is_some_and(|entry| entry.is_file()) {
                    return Err(format!(
                        "expected a regular Claude cache marker {}",
                        path.display()
                    ));
                }
                continue;
            }
            // Receipt keys use portable forward slashes.
            let name = relative
                .components()
                .map(|part| part.as_os_str().to_str().unwrap_or(""))
                .collect::<Vec<_>>()
                .join("/");
            if !dirs.contains(&relative) && !expected.contains_key(&name) {
                return Err(format!("unexpected entry {}", entry.path().display()));
            }
        }
    }
    for (name, content) in expected {
        let path = root.join(name);
        if read_file(&path)? != *content {
            return Err(format!("managed file was edited: {}", path.display()));
        }
    }
    Ok(())
}

fn write_files(root: &Path, files: &Files) -> Result<()> {
    for (name, content) in files {
        let path = root.join(name);
        metadata(&path)?;
        fs_result(&path, secure_file::write_atomic(&path, content))?;
    }
    Ok(())
}

// Only use this for the selected profile or platform state home, before
// appending manager-controlled names. Missing suffixes retain their spelling.
pub(super) fn resolve_root(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() || path.components().any(|part| part == Component::ParentDir) {
        return Err(format!(
            "expected an absolute directory without parent traversal: {}",
            path.display()
        ));
    }
    let mut missing = Vec::new();
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(_) => {
                let canonical = fs_result(ancestor, ancestor.canonicalize())?;
                let mut root = provider_path(&canonical).into_owned();
                if !fs_result(&root, fs::metadata(&root))?.is_dir() {
                    return Err(format!("expected directory {}", root.display()));
                }
                root.extend(missing.into_iter().rev());
                return Ok(root);
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                if let Some(name) = ancestor.file_name() {
                    missing.push(name);
                }
            }
            Err(error) => return fs_result(ancestor, Err(error)),
        }
    }
    Err(format!("no existing ancestor for {}", path.display()))
}

// Only accept the resolved cache or the exact spelling under the selected
// logical profile. The authoritative cache path must have no symlink below
// the resolved root. Arbitrary provider-reported aliases are rejected.
pub(super) fn matches_install_path(
    reported: &Path,
    expected: &Path,
    logical: &Path,
) -> Result<bool> {
    let reported = provider_path(reported);
    if (reported != expected && reported != logical)
        || !metadata(expected)?.is_some_and(|entry| entry.is_dir())
    {
        return Ok(false);
    }
    if reported == expected {
        return Ok(true);
    }
    let canonical = fs_result(&reported, reported.canonicalize())?;
    Ok(provider_path(&canonical) == expected)
}

#[cfg(not(windows))]
pub(super) fn provider_path(path: &Path) -> Cow<'_, Path> {
    Cow::Borrowed(path)
}

#[cfg(windows)]
pub(super) fn provider_path(path: &Path) -> Cow<'_, Path> {
    use std::ffi::OsString;
    use std::path::Prefix;

    let mut components = path.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return Cow::Borrowed(path);
    };
    let root = match prefix.kind() {
        Prefix::VerbatimDisk(drive) => OsString::from(format!("{}:\\", char::from(drive))),
        Prefix::VerbatimUNC(server, share) => {
            let mut root = OsString::from(r"\\");
            root.push(server);
            root.push(r"\");
            root.push(share);
            root.push(r"\");
            root
        }
        _ => return Cow::Borrowed(path),
    };
    if components.next() != Some(Component::RootDir) {
        return Cow::Borrowed(path);
    }
    Cow::Owned(PathBuf::from(root).join(components.as_path()))
}

pub(super) fn initialize_profile(profile: &Path) -> Result<()> {
    let child = profile.join("plugins");
    fs_result(profile, secure_file::create_parent_dir(&child))?;
    if !metadata(profile)?.is_some_and(|entry| entry.is_dir()) {
        return Err(format!(
            "expected provider profile directory {}",
            profile.display()
        ));
    }
    Ok(())
}

pub(super) fn metadata(path: &Path) -> Result<Option<Metadata>> {
    if !path.is_absolute() || path.components().any(|part| part == Component::ParentDir) {
        return Err(format!(
            "expected an absolute path without parent traversal: {}",
            path.display()
        ));
    }
    for ancestor in path.ancestors().collect::<Vec<_>>().into_iter().rev() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(format!("symlink in managed path {}", ancestor.display()));
            }
            Ok(metadata) if ancestor == path => return Ok(Some(metadata)),
            Ok(metadata) if !metadata.is_dir() => {
                return Err(format!("not a directory: {}", ancestor.display()));
            }
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return fs_result(ancestor, Err(error)),
        }
    }
    Ok(None)
}

fn read_file(path: &Path) -> Result<String> {
    if !metadata(path)?.is_some_and(|metadata| metadata.is_file()) {
        return Err(format!(
            "expected a managed regular file {}",
            path.display()
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut data = String::new();
    let file = fs_result(path, options.open(path))?;
    fs_result(path, file.take(MAX_FILE + 1).read_to_string(&mut data))?;
    if data.len() as u64 > MAX_FILE {
        return Err(format!("managed file is too large: {}", path.display()));
    }
    Ok(data)
}

fn fs_result<T>(path: &Path, result: std::io::Result<T>) -> Result<T> {
    result.map_err(|error| format!("{}: {error}", path.display()))
}

// Check for files the provider might remove along with its cache namespace.
pub(super) fn verify_cache_root(
    root: &Path,
    mut verify_version: impl FnMut(&std::ffi::OsStr) -> Result<()>,
) -> Result<()> {
    let Some(entry) = metadata(root)? else {
        return Ok(());
    };
    if !entry.is_dir() {
        return Err(format!("expected cache directory {}", root.display()));
    }
    for entry in fs_result(root, fs::read_dir(root))? {
        let entry = fs_result(root, entry)?;
        if entry.file_name() != "mcp-repl" {
            return Err(format!("unexpected cache entry {}", entry.path().display()));
        }
        // Reconstruct the validated fixed name rather than trusting DirEntry's path.
        let path = root.join("mcp-repl");
        if !metadata(&path)?.is_some_and(|entry| entry.is_dir()) {
            return Err(format!("expected cache directory {}", path.display()));
        }
        for version in fs_result(&path, fs::read_dir(&path))? {
            verify_version(&fs_result(&path, version)?.file_name())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_manifests_use_the_content_version() {
        let version = bundle_version();
        assert!(version.contains("bundle."));
        for provider in [Provider::Claude, Provider::Codex] {
            let files = plugin_files(&bundle(provider, &version));
            for (name, body) in &files {
                if name.ends_with("plugin.json") {
                    let manifest: serde_json::Value = serde_json::from_str(body).unwrap();
                    assert_eq!(manifest["version"], version);
                }
            }
            assert!(files["skills/mcp-repl/SKILL.md"].contains("describe <name>"));
        }
    }

    #[test]
    fn bundled_driver_uses_the_crate_version() {
        let version = bundle_version();
        let minimum = format!("MIN_REPL_VERSION = \"{}\"", env!("CARGO_PKG_VERSION"));
        for provider in [Provider::Claude, Provider::Codex] {
            let files = plugin_files(&bundle(provider, &version));
            let driver = &files["skills/mcp-repl/scripts/session_driver.py"];
            assert!(driver.lines().any(|line| line == minimum));
            assert!(!driver.contains("@SESSION_DRIVER_MIN_REPL_VERSION@"));
            assert!(!driver.contains(&version));
        }
    }

    #[test]
    fn source_rejects_foreign_and_edited_files_and_serializes_writers() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let profile = base.join("profile");
        let state = State::new(&base, Provider::Codex);
        let version = bundle_version();
        let files = bundle(Provider::Codex, &version);
        let lock = state.lock(true).unwrap();
        assert!(state.lock(true).is_err());
        state.create_source(&files).unwrap();
        assert!(state.read(&profile).unwrap_err().contains("foreign source"));
        let receipt = Receipt {
            schema: 1,
            profile: profile.clone(),
            caches: BTreeMap::from([(version.clone(), plugin_files(&files))]),
            version,
            files,
            pending: None,
        };
        state.save(&receipt).unwrap();
        assert!(state.read(&profile).unwrap().is_some());
        let unrelated = state.source.join("unrelated");
        fs::write(&unrelated, "keep").unwrap();
        assert!(state.remove(&receipt.files).is_err());
        assert_eq!(fs::read_to_string(&unrelated).unwrap(), "keep");
        fs::remove_file(unrelated).unwrap();
        let skill = state
            .source
            .join("plugins/mcp-repl/skills/mcp-repl/SKILL.md");
        fs::write(&skill, "user edit").unwrap();
        assert!(
            state
                .replace_source(&receipt.files, &receipt.files)
                .is_err()
        );
        assert_eq!(fs::read_to_string(skill).unwrap(), "user edit");
        drop(lock);
        assert!(state.lock(true).is_ok());
    }

    #[test]
    fn only_claude_cache_versions_allow_a_regular_root_orphan_marker() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let files = plugin_files(&bundle(Provider::Claude, &bundle_version()));
        write_files(&root, &files).unwrap();

        let marker = root.join(".orphaned_at");
        fs::write(&marker, "provider timestamp").unwrap();
        assert!(verify_cache_version(Provider::Claude, &root, &files).is_ok());
        assert!(verify_cache_version(Provider::Codex, &root, &files).is_err());
        assert!(verify_tree(&root, &files).is_err());

        let nested = root.join("skills/.orphaned_at");
        fs::write(&nested, "unexpected file").unwrap();
        assert!(verify_cache_version(Provider::Claude, &root, &files).is_err());
        fs::remove_file(nested).unwrap();

        fs::remove_file(&marker).unwrap();
        fs::create_dir(&marker).unwrap();
        assert!(verify_cache_version(Provider::Claude, &root, &files).is_err());
        #[cfg(unix)]
        {
            fs::remove_dir(&marker).unwrap();
            std::os::unix::fs::symlink(root.join("plugin.json"), marker).unwrap();
            assert!(verify_cache_version(Provider::Claude, &root, &files).is_err());
        }
    }

    #[test]
    fn failed_install_preparation_leaves_source_and_receipt_absent() {
        let temp = tempfile::tempdir().unwrap();
        let base = provider_path(temp.path()).into_owned();
        let base = resolve_root(&base).unwrap();
        let profile = base.join("profile");
        let state = State::new(&base, Provider::Codex);
        let pending = Receipt {
            schema: 1,
            profile: profile.clone(),
            files: Default::default(),
            version: String::new(),
            caches: Default::default(),
            pending: Some("install".into()),
        };
        // The first write succeeds, but its file blocks the second write.
        let files = Files::from([
            ("collision".into(), "first".into()),
            ("collision/child".into(), "second".into()),
        ]);
        assert!(state.install_source(&pending, &files).is_err());
        assert!(state.read(&profile).unwrap().is_none());
        assert_eq!(fs::read_dir(base).unwrap().count(), 0);
    }

    #[cfg(windows)]
    #[test]
    fn normalizes_windows_verbatim_drive_and_unc_paths() {
        use std::borrow::Cow;

        for (verbatim, plain) in [
            (r"\\?\C:\", r"C:\"),
            (r"\\?\D:\café\技能", r"D:\café\技能"),
            (
                r"\\?\UNC\server\share\café\技能",
                r"\\server\share\café\技能",
            ),
        ] {
            assert_eq!(provider_path(Path::new(verbatim)), Path::new(plain));
        }
        for unchanged in [
            r"C:\path",
            r"\\server\share\path",
            r"\\?\Volume{example}\path",
        ] {
            assert!(matches!(
                provider_path(Path::new(unchanged)),
                Cow::Borrowed(_)
            ));
        }
    }

    #[cfg(unix)]
    #[test]
    fn install_path_accepts_only_selected_profile_alias_and_strict_cache_path() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let profile = root.join("profile");
        let expected = profile.join("plugins/cache/version");
        fs::create_dir_all(&expected).unwrap();
        let alias = root.join(".claude");
        symlink(&profile, &alias).unwrap();
        let logical = alias.join("plugins/cache/version");
        assert!(matches_install_path(&logical, &expected, &logical).unwrap());

        let other = root.join("foreign");
        symlink(&profile, &other).unwrap();
        assert!(
            !matches_install_path(&other.join("plugins/cache/version"), &expected, &logical)
                .unwrap()
        );

        fs::remove_dir(&expected).unwrap();
        symlink(&root, &expected).unwrap();
        assert!(
            matches_install_path(&logical, &expected, &logical)
                .unwrap_err()
                .contains("symlink")
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolves_selected_root_without_trusting_symlinks_below_it() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let target = root.join("target");
        fs::create_dir(&target).unwrap();
        let linked = root.join("linked");
        symlink(&target, &linked).unwrap();
        assert_eq!(resolve_root(&linked).unwrap(), target);
        assert_eq!(
            resolve_root(&linked.join("missing/state")).unwrap(),
            target.join("missing/state")
        );

        let selected = resolve_root(&linked).unwrap();
        symlink(&root, selected.join("mcp-repl")).unwrap();
        assert!(
            State::new(&selected.join("mcp-repl/plugins"), Provider::Codex)
                .lock(true)
                .unwrap_err()
                .contains("symlink")
        );
        symlink(&root, selected.join("plugins")).unwrap();
        assert!(
            metadata(&selected.join("plugins/cache/managed"))
                .unwrap_err()
                .contains("symlink")
        );
    }

    #[test]
    fn validates_pending_receipt_without_inspecting_partially_changed_files() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let profile = root.join("profile");
        let state = State::new(&root, Provider::Codex);
        let mut receipt = Receipt {
            schema: 1,
            profile: profile.clone(),
            files: Default::default(),
            version: String::new(),
            caches: Default::default(),
            pending: Some("install".into()),
        };
        fs::create_dir(&state.source).unwrap();
        fs::write(state.source.join("partial"), "keep").unwrap();
        state.save(&receipt).unwrap();
        assert_eq!(
            state.read(&profile).unwrap().unwrap().pending.as_deref(),
            Some("install")
        );
        assert!(state.read(&root.join("other-profile")).is_err());
        receipt.schema = 2;
        state.save(&receipt).unwrap();
        assert!(state.read(&profile).is_err());
        receipt.schema = 1;
        receipt.pending = Some("invalid".into());
        state.save(&receipt).unwrap();
        assert!(state.read(&profile).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn source_rejects_symlinked_owned_files() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let state = State::new(&base, Provider::Claude);
        let files = bundle(Provider::Claude, &bundle_version());
        state.create_source(&files).unwrap();
        let path = state.source.join("plugins/mcp-repl/plugin.json");
        let outside = base.join("unrelated");
        fs::rename(&path, &outside).unwrap();
        std::os::unix::fs::symlink(&outside, &path).unwrap();
        assert!(state.remove(&files).unwrap_err().contains("symlink"));
        assert!(outside.is_file());
    }
}
