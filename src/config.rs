//! Server profiles: a config file of named servers, so a remote MCP server
//! can be reached as `mcp-repl <name>` instead of a URL plus repeated
//! `--bearer`/`--header` flags.
//!
//! On Unix the file lives at `$XDG_CONFIG_HOME/mcp-repl/config.toml`, falling
//! back to `~/.config/mcp-repl/config.toml`. On Windows it lives below
//! `%APPDATA%\mcp-repl`. `--config <path>` overrides either default:
//!
//! ```toml
//! [servers.cratesio]
//! transport = "http"
//! url = "https://cratesio-mcp.fly.dev/"
//! bearer_env = "CRATESIO_TOKEN"
//! headers = { "X-Api-Key" = "..." }
//!
//! [oauth.work]
//! url = "https://mcp.example.com/mcp"
//! scopes = ["openid", "offline_access"]
//!
//! [servers.work]
//! transport = "http"
//! oauth = "work"
//!
//! [servers.local]
//! transport = "stdio"
//! command = ["cargo", "run", "--example", "getting_started"]
//!
//! [aliases]
//! t = "tools"
//! ```
//!
//! Command aliases live in the same file: `[aliases]` for every server, and
//! `[servers.<name>.aliases]` for one profile. The interactive `alias` and
//! `unalias` commands write them back.
//!
//! Tokens are read from the environment via `bearer_env` rather than stored in
//! the file; an inline `bearer` literal works but warns.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The whole config file: named profiles under `[servers.<name>]`, plus the
/// command aliases every server sees under `[aliases]`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub servers: BTreeMap<String, Profile>,
    /// Non-secret metadata for named OAuth credential profiles. Tokens and
    /// registered client secrets live in the operating-system credential store.
    #[serde(default)]
    pub oauth: BTreeMap<String, OAuthProfile>,
    /// Command aliases in effect against every server.
    #[serde(default)]
    pub aliases: BTreeMap<String, String>,
    /// Settings for the REPL itself rather than for any one server.
    #[serde(default)]
    pub repl: Repl,
}

/// The `[repl]` table: knobs that are not about a connection.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Repl {
    /// How many lines of command history to keep. `0` disables persistence
    /// as surely as `--no-history` does.
    pub history_capacity: Option<usize>,
    /// Seconds to allow a request before giving up, when `--timeout` does not
    /// say. `0` waits indefinitely, exactly as the flag's `0` does.
    pub request_timeout: Option<u64>,
    /// Milliseconds to wait for a server to answer `completion/complete`
    /// while the user is mid-word. Short on purpose: this runs between
    /// keystrokes, and a menu that arrives late is worse than one that does
    /// not arrive.
    pub completion_timeout_ms: Option<u64>,
}

/// One `[servers.<name>]` table.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// `http` or `stdio`. Optional: inferred from `url`/`command` when absent.
    pub transport: Option<Transport>,
    /// The endpoint for an `http` profile.
    pub url: Option<String>,
    /// An inline bearer token. Prefer `bearer_env`; this warns when used.
    pub bearer: Option<String>,
    /// Name of the environment variable holding the bearer token.
    pub bearer_env: Option<String>,
    /// Named entry in the top-level `[oauth]` table.
    pub oauth: Option<String>,
    /// Extra headers sent with every request of an `http` profile.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// The command (and arguments) of a `stdio` profile's child process.
    #[serde(default)]
    pub command: Vec<String>,
    /// Command aliases in effect only through this profile. They shadow the
    /// file-level `[aliases]` of the same name.
    #[serde(default)]
    pub aliases: BTreeMap<String, String>,
}

/// Non-secret OAuth metadata stored under `[oauth.<name>]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OAuthProfile {
    /// MCP protected-resource URL used when this profile was authorized.
    pub url: String,
    /// Initial scopes requested during login and tracked for escalation.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Optional HTTPS Client ID Metadata Document URL (CIMD).
    pub client_id_metadata_document: Option<String>,
    /// Preferred authorization-server issuer when discovery advertises several.
    pub authorization_server: Option<String>,
}

/// The transports a profile can name. `ws` and stateless HTTP are not
/// profile-addressable yet, so an unknown value is a config error rather than
/// a silent fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    Http,
    Stdio,
}

/// A profile resolved into everything needed to connect. Produced after the
/// CLI flags have had their say.
#[derive(Debug, PartialEq, Eq)]
pub enum Connection {
    Http {
        url: String,
        bearer: Option<String>,
        headers: Vec<(String, String)>,
        oauth: Option<String>,
    },
    Stdio {
        command: Vec<String>,
        env: BTreeMap<String, String>,
        cwd: Option<PathBuf>,
    },
}

impl Config {
    /// Parse a config file's contents.
    pub fn parse(source: &str) -> Result<Self, String> {
        toml::from_str(source).map_err(|e| e.to_string())
    }

    /// Read the config from `path`. A missing file is an error only when the
    /// path was explicitly requested (`--config`); the default location is
    /// allowed not to exist.
    pub fn load(path: &Path, explicit: bool) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(source) => {
                // The file may hold an inline bearer or an Authorization
                // header. One written before this behavior existed, or by
                // an older release, is tightened here rather than waiting
                // for the next write to replace it.
                crate::secure_file::restrict_existing(path);
                Self::parse(&source).map_err(|e| format!("{}: {e}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !explicit => Ok(Self::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    /// Look up a profile by name, with an error listing the known names when
    /// it is missing.
    pub fn profile(&self, name: &str) -> Result<&Profile, String> {
        self.servers.get(name).ok_or_else(|| {
            if self.servers.is_empty() {
                format!("no server profile named {name:?}: no profiles are configured")
            } else {
                format!(
                    "no server profile named {name:?}: known profiles are {}",
                    self.names().join(", ")
                )
            }
        })
    }

    /// The configured profile names, sorted.
    pub fn names(&self) -> Vec<&str> {
        self.servers.keys().map(String::as_str).collect()
    }

    /// Resolve a named server, allowing an OAuth-only server profile to reuse
    /// the protected-resource URL saved with its credential profile.
    pub fn resolve_profile_with(
        &self,
        name: &str,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<Connection, String> {
        let profile = self.profile(name)?;
        let oauth_url = profile
            .oauth
            .as_deref()
            .map(|oauth| {
                self.oauth
                    .get(oauth)
                    .map(|metadata| metadata.url.as_str())
                    .ok_or_else(|| {
                        format!("server profile references unknown OAuth profile {oauth:?}")
                    })
            })
            .transpose()?;
        profile.resolve_with_oauth_url(lookup, oauth_url)
    }
}

impl Profile {
    /// The transport this profile connects over: the declared one, else
    /// inferred from whichever of `url`/`command` is present.
    pub fn transport(&self) -> Result<Transport, String> {
        match (
            self.transport,
            self.url.is_some() || self.oauth.is_some(),
            !self.command.is_empty(),
        ) {
            (Some(t), _, _) => Ok(t),
            (None, true, false) => Ok(Transport::Http),
            (None, false, true) => Ok(Transport::Stdio),
            (None, true, true) => Err(
                "profile sets both `url` and `command`: add `transport = \"http\"` or \
                 `transport = \"stdio\"` to say which one applies"
                    .to_string(),
            ),
            (None, false, false) => {
                Err("profile has neither `url` nor `command`, so it cannot connect".to_string())
            }
        }
    }

    /// The bearer token for this profile: `bearer_env` read from `lookup`, or
    /// the inline `bearer`. A `bearer_env` naming an unset variable is an
    /// error, not a silent anonymous connection.
    pub fn bearer_token_with(
        &self,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<Option<String>, String> {
        if let Some(var) = &self.bearer_env {
            return lookup(var).map(Some).ok_or_else(|| {
                format!(
                    "profile sets `bearer_env = {var:?}` but that environment variable is unset"
                )
            });
        }
        Ok(self.bearer.clone())
    }

    /// Resolve into a [`Connection`], validating that the transport has the
    /// fields it needs.
    #[cfg(test)]
    pub fn resolve_with(
        &self,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<Connection, String> {
        self.resolve_with_oauth_url(lookup, None)
    }

    fn resolve_with_oauth_url(
        &self,
        lookup: impl Fn(&str) -> Option<String>,
        oauth_url: Option<&str>,
    ) -> Result<Connection, String> {
        match self.transport()? {
            Transport::Http => {
                if self.oauth.is_some()
                    && (self.bearer.is_some()
                        || self.bearer_env.is_some()
                        || self
                            .headers
                            .keys()
                            .any(|name| name.eq_ignore_ascii_case("authorization")))
                {
                    return Err(
                        "HTTP profile cannot combine `oauth` with `bearer`, `bearer_env`, or an \
                         Authorization header"
                            .to_string(),
                    );
                }
                let url = self
                    .url
                    .clone()
                    .or_else(|| oauth_url.map(str::to_string))
                    .ok_or("profile has `transport = \"http\"` but no `url`")?;
                Ok(Connection::Http {
                    url,
                    bearer: self.bearer_token_with(lookup)?,
                    headers: self
                        .headers
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect(),
                    oauth: self.oauth.clone(),
                })
            }
            Transport::Stdio => {
                if self.command.is_empty() {
                    return Err("profile has `transport = \"stdio\"` but no `command`".to_string());
                }
                Ok(Connection::Stdio {
                    command: self.command.clone(),
                    env: BTreeMap::new(),
                    cwd: None,
                })
            }
        }
    }

    /// A one-line summary for `--list-servers`.
    pub fn summary(&self) -> String {
        match self.transport() {
            Ok(Transport::Http) => format!(
                "http   {}",
                self.url
                    .as_deref()
                    .or(self.oauth.as_deref())
                    .unwrap_or("(no url)")
            ),
            Ok(Transport::Stdio) => format!("stdio  {}", self.command.join(" ")),
            Err(e) => format!("(invalid: {e})"),
        }
    }
}

/// The config file location: `--config` if given, else
/// the platform-native mcp-repl config directory.
/// The bool is true when the path was explicitly requested, which makes a
/// missing file an error.
pub fn config_path(explicit: Option<&str>) -> Option<(PathBuf, bool)> {
    config_path_with(explicit, &crate::directories::Directories::current())
}

fn config_path_with(
    explicit: Option<&str>,
    directories: &crate::directories::Directories,
) -> Option<(PathBuf, bool)> {
    if let Some(p) = explicit {
        return Some((PathBuf::from(p), true));
    }
    Some((directories.config_file()?, false))
}

#[cfg(test)]
mod tests {
    /// The example config at the repository root, parsed as a fixture.
    ///
    /// `deny_unknown_fields` means a key that drifts away from the parser is a
    /// hard failure rather than a line that quietly does nothing, so parsing
    /// the example here is what keeps it honest. Included at compile time, so
    /// deleting or renaming it breaks the build rather than skipping the test.
    const EXAMPLE: &str = include_str!("../config.example.toml");

    #[test]
    fn the_example_config_parses() {
        let config = Config::parse(EXAMPLE).expect("config.example.toml parses");
        assert_eq!(
            config.servers.len(),
            4,
            "four profiles, one per shape shown"
        );
        assert_eq!(config.oauth.len(), 1);
        assert!(!config.aliases.is_empty());
    }

    /// Every key the parser accepts appears in the example.
    ///
    /// Without this the example decays one field at a time: a new key is
    /// added, nobody documents it, and the file quietly stops being complete
    /// while still parsing perfectly.
    #[test]
    fn the_example_config_exercises_every_key() {
        let config = Config::parse(EXAMPLE).expect("parses");

        // Profile: each optional key set by at least one profile, and each
        // collection non-empty somewhere.
        let servers: Vec<&Profile> = config.servers.values().collect();
        let any = |f: &dyn Fn(&Profile) -> bool| servers.iter().any(|p| f(p));
        assert!(any(&|p| p.transport.is_some()), "transport");
        assert!(any(&|p| p.url.is_some()), "url");
        assert!(any(&|p| p.bearer.is_some()), "bearer");
        assert!(any(&|p| p.bearer_env.is_some()), "bearer_env");
        assert!(any(&|p| p.oauth.is_some()), "oauth");
        assert!(any(&|p| !p.headers.is_empty()), "headers");
        assert!(any(&|p| !p.command.is_empty()), "command");
        assert!(any(&|p| !p.aliases.is_empty()), "profile aliases");

        // Both transports are represented, since they are configured
        // differently and showing only one leaves the other undocumented.
        assert!(
            servers.iter().any(|p| !p.command.is_empty())
                && servers.iter().any(|p| p.url.is_some()),
            "a stdio profile and an http profile"
        );

        let oauth = config.oauth.values().next().expect("an oauth profile");
        assert!(!oauth.url.is_empty(), "oauth url");
        assert!(!oauth.scopes.is_empty(), "oauth scopes");
        assert!(
            oauth.client_id_metadata_document.is_some(),
            "client_id_metadata_document"
        );
        assert!(oauth.authorization_server.is_some(), "authorization_server");

        assert!(config.repl.history_capacity.is_some(), "history_capacity");
        assert!(config.repl.request_timeout.is_some(), "request_timeout");
        assert!(
            config.repl.completion_timeout_ms.is_some(),
            "completion_timeout_ms"
        );
    }

    use super::*;
    use crate::directories::{Directories, Platform};
    use std::ffi::OsString;

    #[test]
    fn windows_directories_drive_the_default_config_path() {
        let directories = Directories::from_lookup(Platform::Windows, |name| {
            (name == "APPDATA").then(|| OsString::from(r"C:\Users\Ada\AppData\Roaming"))
        });
        assert_eq!(
            config_path_with(None, &directories),
            Some((
                PathBuf::from(r"C:\Users\Ada\AppData\Roaming")
                    .join("mcp-repl")
                    .join("config.toml"),
                false
            ))
        );
        assert_eq!(
            config_path_with(Some("portable.toml"), &directories),
            Some((PathBuf::from("portable.toml"), true))
        );
    }

    const SAMPLE: &str = r#"
[servers.cratesio]
transport = "http"
url = "https://cratesio-mcp.fly.dev/"
bearer_env = "CRATESIO_TOKEN"
headers = { "X-Api-Key" = "abc" }

[servers.local]
transport = "stdio"
command = ["cargo", "run", "--example", "getting_started"]
"#;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: BTreeMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| map.get(k).cloned()
    }

    #[test]
    fn the_repl_table_is_optional_and_defaults_to_nothing_set() {
        // Absent section, absent keys: every consumer falls back to its own
        // default, so adding a key here cannot change behaviour by itself.
        let config: Config = toml::from_str(SAMPLE).expect("parses");
        assert_eq!(config.repl.history_capacity, None);
        assert_eq!(config.repl.request_timeout, None);
        assert_eq!(config.repl.completion_timeout_ms, None);
    }

    #[test]
    fn the_repl_table_parses_its_tunables() {
        let config: Config = toml::from_str(
            r#"
[repl]
history_capacity = 50
request_timeout = 7
completion_timeout_ms = 250
"#,
        )
        .expect("parses");
        assert_eq!(config.repl.history_capacity, Some(50));
        assert_eq!(config.repl.request_timeout, Some(7));
        assert_eq!(config.repl.completion_timeout_ms, Some(250));
    }

    /// A silently ignored typo in a config file is worse than a refusal: the
    /// setting appears to be applied and is not.
    #[test]
    fn a_misspelled_repl_key_is_refused_and_names_the_alternatives() {
        let error = toml::from_str::<Config>("[repl]\nhistory_capacty = 50\n")
            .expect_err("a typo is an error");
        let message = error.to_string();
        assert!(message.contains("history_capacty"), "{message}");
        assert!(message.contains("history_capacity"), "{message}");
    }

    /// `0` is a value, not an absence: it means "wait indefinitely" for the
    /// timeout and "keep no history" for the capacity.
    #[test]
    fn zero_is_a_setting_rather_than_an_unset_key() {
        let config: Config =
            toml::from_str("[repl]\nhistory_capacity = 0\nrequest_timeout = 0\n").expect("parses");
        assert_eq!(config.repl.history_capacity, Some(0));
        assert_eq!(config.repl.request_timeout, Some(0));
    }

    #[test]
    fn parses_named_profiles() {
        let config = Config::parse(SAMPLE).unwrap();
        assert_eq!(config.names(), vec!["cratesio", "local"]);
    }

    #[test]
    fn http_profile_resolves_transport_and_auth() {
        let config = Config::parse(SAMPLE).unwrap();
        let resolved = config
            .profile("cratesio")
            .unwrap()
            .resolve_with(env(&[("CRATESIO_TOKEN", "secret")]))
            .unwrap();
        assert_eq!(
            resolved,
            Connection::Http {
                url: "https://cratesio-mcp.fly.dev/".to_string(),
                bearer: Some("secret".to_string()),
                headers: vec![("X-Api-Key".to_string(), "abc".to_string())],
                oauth: None,
            }
        );
    }

    #[test]
    fn oauth_metadata_and_server_selection_are_non_secret() {
        let config = Config::parse(
            r#"
[oauth.work]
url = "https://mcp.example/mcp"
scopes = ["openid", "offline_access"]
client_id_metadata_document = "https://client.example/metadata.json"
authorization_server = "https://auth.example"

[servers.work]
oauth = "work"
headers = { "X-Tenant" = "acme" }
"#,
        )
        .unwrap();

        assert_eq!(config.oauth["work"].scopes, ["openid", "offline_access"]);
        assert_eq!(
            config.resolve_profile_with("work", env(&[])).unwrap(),
            Connection::Http {
                url: "https://mcp.example/mcp".to_string(),
                bearer: None,
                headers: vec![("X-Tenant".to_string(), "acme".to_string())],
                oauth: Some("work".to_string()),
            }
        );
    }

    #[test]
    fn unknown_oauth_reference_is_an_actionable_error() {
        let config = Config::parse("[servers.work]\noauth = \"missing\"\n").unwrap();
        let error = config.resolve_profile_with("work", env(&[])).unwrap_err();
        assert!(
            error.contains("unknown OAuth profile \"missing\""),
            "{error}"
        );
    }

    #[test]
    fn oauth_server_profile_rejects_ambiguous_static_auth() {
        for auth in [
            "bearer = \"secret\"",
            "bearer_env = \"TOKEN\"",
            "headers = { Authorization = \"Bearer secret\" }",
        ] {
            let source = format!(
                "[servers.work]\nurl = \"https://mcp.example/mcp\"\noauth = \"work\"\n{auth}\n"
            );
            let error = Config::parse(&source)
                .unwrap()
                .profile("work")
                .unwrap()
                .resolve_with(env(&[("TOKEN", "secret")]))
                .unwrap_err();
            assert!(error.contains("cannot combine `oauth`"), "{error}");
        }
    }

    #[test]
    fn stdio_profile_resolves_command() {
        let config = Config::parse(SAMPLE).unwrap();
        let resolved = config
            .profile("local")
            .unwrap()
            .resolve_with(env(&[]))
            .unwrap();
        assert_eq!(
            resolved,
            Connection::Stdio {
                command: vec![
                    "cargo".to_string(),
                    "run".to_string(),
                    "--example".to_string(),
                    "getting_started".to_string(),
                ],
                env: BTreeMap::new(),
                cwd: None,
            }
        );
    }

    #[test]
    fn unknown_profile_lists_known_names() {
        let config = Config::parse(SAMPLE).unwrap();
        let err = config.profile("nope").unwrap_err();
        assert!(err.contains("nope"), "{err}");
        assert!(err.contains("cratesio, local"), "{err}");
    }

    #[test]
    fn unknown_profile_with_empty_config_says_so() {
        let err = Config::default().profile("nope").unwrap_err();
        assert!(err.contains("no profiles are configured"), "{err}");
    }

    #[test]
    fn unset_bearer_env_is_an_error() {
        let config = Config::parse(SAMPLE).unwrap();
        let err = config
            .profile("cratesio")
            .unwrap()
            .resolve_with(env(&[]))
            .unwrap_err();
        assert!(err.contains("CRATESIO_TOKEN"), "{err}");
    }

    #[test]
    fn inline_bearer_is_used_when_no_env_indirection() {
        let profile: Profile = toml::from_str(
            r#"
            url = "https://example/mcp"
            bearer = "literal"
            "#,
        )
        .unwrap();
        assert_eq!(
            profile.bearer_token_with(env(&[])).unwrap(),
            Some("literal".to_string())
        );
    }

    #[test]
    fn transport_is_inferred_from_the_fields() {
        let http: Profile = toml::from_str(r#"url = "https://example/mcp""#).unwrap();
        assert_eq!(http.transport().unwrap(), Transport::Http);
        let stdio: Profile = toml::from_str(r#"command = ["server"]"#).unwrap();
        assert_eq!(stdio.transport().unwrap(), Transport::Stdio);
    }

    #[test]
    fn ambiguous_and_empty_profiles_are_errors() {
        let both: Profile =
            toml::from_str("url = \"https://example/mcp\"\ncommand = [\"server\"]").unwrap();
        assert!(both.transport().unwrap_err().contains("both"));
        assert!(
            Profile::default()
                .transport()
                .unwrap_err()
                .contains("neither")
        );
    }

    #[test]
    fn declared_transport_must_have_its_fields() {
        let profile: Profile = toml::from_str(r#"transport = "http""#).unwrap();
        assert!(profile.resolve_with(env(&[])).unwrap_err().contains("url"));
        let profile: Profile = toml::from_str(r#"transport = "stdio""#).unwrap();
        assert!(
            profile
                .resolve_with(env(&[]))
                .unwrap_err()
                .contains("command")
        );
    }

    #[test]
    fn an_unsupported_transport_names_itself() {
        let err =
            Config::parse("[servers.x]\ntransport = \"ws\"\nurl = \"wss://example\"").unwrap_err();
        assert!(err.contains("ws"), "{err}");
    }

    #[test]
    fn aliases_parse_at_both_scopes() {
        let config = Config::parse(
            r#"
[aliases]
t = "tools"

[servers.cratesio]
url = "https://cratesio-mcp.fly.dev/"
aliases = { dl = "get_downloads crate" }
"#,
        )
        .unwrap();
        assert_eq!(config.aliases.get("t").map(String::as_str), Some("tools"));
        assert_eq!(
            config.servers["cratesio"]
                .aliases
                .get("dl")
                .map(String::as_str),
            Some("get_downloads crate")
        );
    }

    #[test]
    fn a_config_without_aliases_parses_to_none_of_them() {
        assert!(Config::parse(SAMPLE).unwrap().aliases.is_empty());
    }

    #[test]
    fn a_typo_in_a_profile_key_is_rejected() {
        let err =
            Config::parse("[servers.x]\nurl = \"https://example\"\nbearrer = \"x\"").unwrap_err();
        assert!(err.contains("bearrer"), "{err}");
    }
}
