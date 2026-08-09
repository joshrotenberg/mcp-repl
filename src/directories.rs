//! Platform-aware locations for mcp-repl's user files.
//!
//! Keep environment interpretation here so config, history, OAuth metadata,
//! aliases, and imported-client discovery cannot quietly disagree about what
//! a user's home or application directories are.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Platform {
    Windows,
    Macos,
    Unix,
}

impl Platform {
    fn current() -> Self {
        match std::env::consts::OS {
            "windows" => Self::Windows,
            "macos" => Self::Macos,
            _ => Self::Unix,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Directories {
    platform: Platform,
    home: Option<PathBuf>,
    config: Option<PathBuf>,
    state: Option<PathBuf>,
    roaming: Option<PathBuf>,
}

impl Directories {
    pub(crate) fn current() -> Self {
        Self::from_lookup(Platform::current(), |name| std::env::var_os(name))
    }

    pub(crate) fn from_lookup(
        platform: Platform,
        mut lookup: impl FnMut(&str) -> Option<OsString>,
    ) -> Self {
        let mut get = |name| {
            lookup(name)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        };

        match platform {
            Platform::Windows => {
                let home = get("USERPROFILE").or_else(|| get("HOME"));
                let roaming = get("APPDATA").or_else(|| {
                    home.as_ref()
                        .map(|path| path.join("AppData").join("Roaming"))
                });
                let state = get("LOCALAPPDATA")
                    .or_else(|| home.as_ref().map(|path| path.join("AppData").join("Local")));
                Self {
                    platform,
                    home,
                    config: roaming.clone(),
                    state,
                    roaming,
                }
            }
            Platform::Macos | Platform::Unix => {
                let home = get("HOME");
                let config = get("XDG_CONFIG_HOME")
                    .or_else(|| home.as_ref().map(|path| path.join(".config")));
                let state = get("XDG_STATE_HOME")
                    .or_else(|| home.as_ref().map(|path| path.join(".local").join("state")));
                Self {
                    platform,
                    home,
                    config,
                    state,
                    roaming: None,
                }
            }
        }
    }

    /// Override only the home used for legacy and imported-client paths.
    /// This preserves the public import API that accepts an explicit home,
    /// while Windows' independent APPDATA discovery remains available.
    pub(crate) fn with_home(mut self, home: Option<PathBuf>) -> Self {
        self.home = home;
        self
    }

    pub(crate) fn home(&self) -> Option<&Path> {
        self.home.as_deref()
    }

    pub(crate) fn config_file(&self) -> Option<PathBuf> {
        Some(self.config.as_ref()?.join("mcp-repl").join("config.toml"))
    }

    pub(crate) fn history_file(&self) -> Option<PathBuf> {
        Some(self.state.as_ref()?.join("mcp-repl").join("history"))
    }

    pub(crate) fn legacy_history_file(&self) -> Option<PathBuf> {
        Some(self.home.as_ref()?.join(".mcp-repl_history"))
    }

    pub(crate) fn claude_desktop_config(&self) -> Option<PathBuf> {
        let path = match self.platform {
            Platform::Windows => self.roaming.as_ref()?.join("Claude"),
            Platform::Macos => self
                .home
                .as_ref()?
                .join("Library")
                .join("Application Support")
                .join("Claude"),
            Platform::Unix => self.home.as_ref()?.join(".config").join("Claude"),
        };
        Some(path.join("claude_desktop_config.json"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn dirs(platform: Platform, pairs: &[(&str, &str)]) -> Directories {
        let values: BTreeMap<&str, OsString> = pairs
            .iter()
            .map(|(key, value)| (*key, OsString::from(value)))
            .collect();
        Directories::from_lookup(platform, |key| values.get(key).cloned())
    }

    #[test]
    fn windows_directories_use_native_config_state_and_home_variables() {
        let dirs = dirs(
            Platform::Windows,
            &[
                ("HOME", "/wrong/unix/home"),
                ("USERPROFILE", r"C:\Users\Ada"),
                ("APPDATA", r"C:\Users\Ada\AppData\Roaming"),
                ("LOCALAPPDATA", r"C:\Users\Ada\AppData\Local"),
                ("XDG_CONFIG_HOME", "/wrong/xdg/config"),
                ("XDG_STATE_HOME", "/wrong/xdg/state"),
            ],
        );

        assert_eq!(dirs.home(), Some(Path::new(r"C:\Users\Ada")));
        assert_eq!(
            dirs.config_file(),
            Some(
                PathBuf::from(r"C:\Users\Ada\AppData\Roaming")
                    .join("mcp-repl")
                    .join("config.toml")
            )
        );
        assert_eq!(
            dirs.history_file(),
            Some(
                PathBuf::from(r"C:\Users\Ada\AppData\Local")
                    .join("mcp-repl")
                    .join("history")
            )
        );
    }

    #[test]
    fn windows_directories_fall_back_from_userprofile_when_appdata_is_missing() {
        let dirs = dirs(Platform::Windows, &[("USERPROFILE", r"C:\Users\Ada")]);
        assert_eq!(
            dirs.config_file(),
            Some(
                PathBuf::from(r"C:\Users\Ada")
                    .join("AppData")
                    .join("Roaming")
                    .join("mcp-repl")
                    .join("config.toml")
            )
        );
        assert_eq!(
            dirs.history_file(),
            Some(
                PathBuf::from(r"C:\Users\Ada")
                    .join("AppData")
                    .join("Local")
                    .join("mcp-repl")
                    .join("history")
            )
        );
    }

    #[test]
    fn unix_directories_preserve_xdg_and_home_fallbacks() {
        let explicit = dirs(
            Platform::Unix,
            &[
                ("HOME", "/home/ada"),
                ("XDG_CONFIG_HOME", "/config"),
                ("XDG_STATE_HOME", "/state"),
            ],
        );
        assert_eq!(
            explicit.config_file(),
            Some(PathBuf::from("/config/mcp-repl/config.toml"))
        );
        assert_eq!(
            explicit.history_file(),
            Some(PathBuf::from("/state/mcp-repl/history"))
        );

        let fallback = dirs(Platform::Unix, &[("HOME", "/home/ada")]);
        assert_eq!(
            fallback.config_file(),
            Some(PathBuf::from("/home/ada/.config/mcp-repl/config.toml"))
        );
        assert_eq!(
            fallback.history_file(),
            Some(PathBuf::from("/home/ada/.local/state/mcp-repl/history"))
        );
    }
}
