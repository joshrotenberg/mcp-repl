//! Local files written by the REPL: the config file, and the command
//! history.
//!
//! Both can hold material the user would not want readable by other
//! accounts on the machine. The config file may carry an inline `bearer`
//! or an `Authorization` header, and the history records every line
//! typed, which routinely includes arguments like
//! `call issue_create {"token": "..."}`. Neither is a credential store
//! (OAuth secrets live in the platform keyring), but both default to
//! owner-only permissions rather than whatever the umask allows.

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
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(OWNER_ONLY_DIR)
            .create(parent)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(parent)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    #[cfg(unix)]
    fn written_files_are_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.toml");
        write_atomic(&path, "[servers]\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[servers]\n");
        assert_eq!(mode_of(&path) & 0o077, 0, "group/other bits must be clear");
    }

    #[test]
    #[cfg(unix)]
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
    #[cfg(unix)]
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
    #[cfg(unix)]
    fn a_new_history_file_is_created_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state").join("history");
        ensure_owner_only(&path).unwrap();
        assert!(path.exists());
        assert_eq!(mode_of(&path) & 0o077, 0);
        assert_eq!(mode_of(path.parent().unwrap()) & 0o077, 0);
    }
}
