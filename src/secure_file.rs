//! Local files written by the REPL: the config file, the command history,
//! and tool results saved by `--max-output`.
//!
//! All of them can hold material the user would not want readable by other
//! accounts on the machine. The config file may carry an inline `bearer`
//! or an `Authorization` header, the history records every line typed,
//! which routinely includes arguments like
//! `call issue_create {"token": "..."}`, and a saved result is whatever a
//! server returned. None is a credential store (OAuth secrets live in the
//! platform keyring), but all default to owner-only permissions rather than
//! whatever the umask allows.
//!
//! Owner-only modes are a unix guarantee. On Windows these files and
//! directories inherit the access control list of the directory they are
//! created in; nothing here changes it.

use std::io::Write;
use std::path::Path;

/// Owner read/write, nothing for group or other.
#[cfg(unix)]
const OWNER_ONLY: u32 = 0o600;
/// Owner-only traversal for directories the REPL creates.
#[cfg(unix)]
const OWNER_ONLY_DIR: u32 = 0o700;

/// Create `path`'s parent directory, owner-only on unix.
pub(crate) fn create_parent_dir(path: &Path) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    if parent.as_os_str().is_empty() || parent.exists() {
        return Ok(());
    }
    create_private_dir(parent)
}

/// Create `path` and any missing parents, owner-only on unix.
pub(crate) fn create_private_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(OWNER_ONLY_DIR)
            .create(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(path)
    }
}

/// Set a directory the REPL just created to owner-only (0700) explicitly,
/// so the umask cannot narrow or widen the mode it was created with. A
/// no-op on non-unix platforms.
pub(crate) fn restrict_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(OWNER_ONLY_DIR))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// Tighten an existing file to owner-only. A file created before this
/// behavior existed, or by an older release, is fixed in place on the
/// next run. Missing files and non-unix platforms are a no-op.
pub(crate) fn restrict_existing(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let Ok(metadata) = std::fs::metadata(path) else {
            return;
        };
        let mode = metadata.permissions().mode();
        if mode & 0o077 != 0 {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(OWNER_ONLY));
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Create `path` owner-only if it does not exist yet, and tighten it if
/// it does. Used for files another library opens for us (the reedline
/// history file), where we control neither the open flags nor the mode.
pub(crate) fn ensure_owner_only(path: &Path) -> std::io::Result<()> {
    create_parent_dir(path)?;
    if !path.exists() {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(OWNER_ONLY);
        }
        match options.open(path) {
            Ok(_) => return Ok(()),
            // A racing process created it first; fall through and tighten.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    restrict_existing(path);
    Ok(())
}

/// Replace `path`'s contents atomically, owner-only.
///
/// The temporary file is created by `tempfile` in the destination
/// directory with a random name and 0600, then renamed over the target:
/// a fixed `<path>.tmp` sibling is both a symlink target an attacker can
/// pre-plant and a collision between two concurrent processes.
pub(crate) fn write_atomic(path: &Path, contents: &str) -> std::io::Result<()> {
    create_parent_dir(path)?;
    let directory = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => std::path::PathBuf::from("."),
    };
    let mut file = tempfile::Builder::new()
        .prefix(".mcp-repl")
        .suffix(".tmp")
        .tempfile_in(&directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(OWNER_ONLY))?;
    }
    file.write_all(contents.as_bytes())?;
    file.flush()?;
    file.persist(path)
        .map_err(|e| std::io::Error::other(format!("{}: {}", path.display(), e.error)))?;
    Ok(())
}

/// Create `path` as a new owner-only file and return it open for writing.
///
/// Used for results saved by `--max-output`. `create_new` fails on any
/// existing entry, including a symlink planted at that name, so an earlier
/// file is never overwritten and a link is never followed. The caller picks
/// another name on `AlreadyExists`. On unix the mode is set to 0600 on the
/// open handle after creation, so the umask cannot change it.
pub(crate) fn create_new_owner_only(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(OWNER_ONLY);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(error) = file.set_permissions(std::fs::Permissions::from_mode(OWNER_ONLY)) {
            drop(file);
            let _ = std::fs::remove_file(path);
            return Err(error);
        }
    }
    Ok(file)
}

/// Write bytes to a new or existing file, owner-only.
///
/// Used by `read --out`: whatever a server serves is as sensitive as the
/// server, so a saved resource does not land at whatever the umask allows.
pub(crate) fn write_bytes(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    create_parent_dir(path)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(OWNER_ONLY);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.flush()?;
    // An existing file keeps its old mode through `open`, so tighten it.
    restrict_existing(path);
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn written_files_are_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.toml");
        write_atomic(&path, "[servers]\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[servers]\n");
        assert_eq!(mode_of(&path) & 0o077, 0, "group/other bits must be clear");
    }

    #[test]
    fn rewriting_keeps_permissions_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_atomic(&path, "first").unwrap();
        write_atomic(&path, "second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        assert_eq!(mode_of(&path) & 0o077, 0);
        let strays: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name() != "config.toml")
            .collect();
        assert!(strays.is_empty(), "temporary file left behind");
    }

    #[test]
    fn an_existing_permissive_file_is_tightened() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history");
        std::fs::write(&path, "echo message=hi\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        ensure_owner_only(&path).unwrap();
        assert_eq!(mode_of(&path) & 0o077, 0);
        // Tightening must not disturb what is already recorded.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "echo message=hi\n");
    }

    #[test]
    fn create_new_refuses_existing_files_and_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let fresh = dir.path().join("fresh.json");
        create_new_owner_only(&fresh).unwrap();
        assert_eq!(mode_of(&fresh), 0o600);

        let existing = dir.path().join("existing.json");
        std::fs::write(&existing, "keep").unwrap();
        let error = create_new_owner_only(&existing).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&existing).unwrap(), "keep");

        let target = dir.path().join("target");
        std::fs::write(&target, "untouched").unwrap();
        let link = dir.path().join("planted.json");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let error = create_new_owner_only(&link).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "untouched");
    }

    #[test]
    fn a_new_history_file_is_created_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state").join("history");
        ensure_owner_only(&path).unwrap();
        assert!(path.exists());
        assert_eq!(mode_of(&path) & 0o077, 0);
        assert_eq!(mode_of(path.parent().unwrap()) & 0o077, 0);
    }
}
