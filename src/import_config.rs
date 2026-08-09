//! Import named servers from the common JSON configuration used by MCP
//! clients such as Claude, Cursor, and VS Code.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::config::Connection;

/// An explicit `PATH:ENTRY` selector recognized by the CLI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selector {
    pub path: PathBuf,
    pub entry: String,
}

/// A resolved imported server plus its display label.
#[derive(Debug, PartialEq, Eq)]
pub struct ImportedConnection {
    pub selector: Selector,
    pub connection: Connection,
    /// Non-secret provenance needed to approve an imported HTTP connection.
    pub http_trust: Option<ImportedHttpTrust>,
}

/// What an imported HTTP entry can forward, without any resolved values.
#[derive(Debug, PartialEq, Eq)]
pub struct ImportedHttpTrust {
    pub header_names: Vec<String>,
    pub header_env_keys: Vec<String>,
    pub url_env_keys: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
struct ResolvedEntry {
    connection: Connection,
    http_trust: Option<ImportedHttpTrust>,
}

impl ImportedConnection {
    pub fn label(&self) -> String {
        format!("{}:{}", self.selector.path.display(), self.selector.entry)
    }
}

/// Recognize an explicit JSON config selector without stealing ordinary
/// executable names containing `:`. A `.json` suffix is explicit even when
/// the file is missing, so the user gets a file error rather than attempting
/// to spawn the whole selector as a command.
pub fn parse_selector(value: &str) -> Option<Result<Selector, String>> {
    let (path, entry) = value.rsplit_once(':')?;
    let path = PathBuf::from(path);
    let looks_like_json = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("json"));
    if !looks_like_json && !path.exists() {
        return None;
    }
    if path.as_os_str().is_empty() {
        return Some(Err("import selector has an empty file path".to_string()));
    }
    if entry.is_empty() {
        return Some(Err(format!(
            "import selector for {} has an empty entry name",
            path.display()
        )));
    }
    Some(Ok(Selector {
        path,
        entry: entry.to_string(),
    }))
}

/// Read and resolve one selected server. Environment values are supplied by
/// the caller so tests never mutate the process environment.
pub fn load_with(
    selector: Selector,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<ImportedConnection, String> {
    let source = std::fs::read_to_string(&selector.path)
        .map_err(|error| format!("{}: {error}", selector.path.display()))?;
    let path = std::fs::canonicalize(&selector.path)
        .map_err(|error| format!("{}: {error}", selector.path.display()))?;
    let resolved = parse_document(&source, &path, &selector.entry, &lookup)?;
    tracing::debug!(
        path = %path.display(),
        entry = %selector.entry,
        "resolved a client config entry"
    );
    Ok(ImportedConnection {
        selector: Selector {
            path,
            entry: selector.entry,
        },
        connection: resolved.connection,
        http_trust: resolved.http_trust,
    })
}

/// Where MCP clients keep their server lists.
///
/// Project files first, since a scan is usually run inside a repository and
/// that is the config the user means. Paths that do not exist are dropped by
/// [`scan`], so this can list every location worth trying.
pub fn candidate_paths(cwd: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let directories = crate::directories::Directories::current()
        .with_home(home.map(std::path::Path::to_path_buf));
    candidate_paths_with(cwd, &directories)
}

pub(crate) fn candidate_paths_with(
    cwd: &Path,
    directories: &crate::directories::Directories,
) -> Vec<PathBuf> {
    let mut paths = vec![
        cwd.join(".mcp.json"),
        cwd.join(".vscode").join("mcp.json"),
        cwd.join(".cursor").join("mcp.json"),
    ];
    if let Some(home) = directories.home() {
        paths.push(home.join(".claude.json"));
        paths.push(home.join(".cursor").join("mcp.json"));
    }
    if let Some(path) = directories.claude_desktop_config() {
        paths.push(path);
    }
    paths
}

/// What one config file yielded.
#[derive(Debug)]
pub struct ScannedFile {
    pub path: PathBuf,
    /// The entries, or why the file could not be read. A malformed file is
    /// reported rather than skipped: silence would read as "nothing here".
    pub result: Result<Vec<DiscoveredEntry>, String>,
}

/// Describe every config that exists among `paths`.
///
/// Files that are absent are skipped; files that exist but cannot be parsed
/// are included with their error.
pub fn scan(paths: &[PathBuf]) -> Vec<ScannedFile> {
    tracing::debug!(candidates = paths.len(), "scanning for client configs");
    paths
        .iter()
        .filter(|path| path.is_file())
        .map(|path| ScannedFile {
            path: path.clone(),
            result: list_entries(path),
        })
        .collect()
}

/// One entry found in a client config, described without being resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredEntry {
    /// The name to put after the colon in a `PATH:ENTRY` selector.
    pub name: String,
    /// `stdio` or `http`, or `?` when the entry declares neither.
    pub transport: String,
    /// The command line or URL, as written in the file.
    pub summary: String,
}

/// List what a config file offers, without resolving it.
///
/// Deliberately not [`load_with`]: resolution expands `${env:NAME}` and fails
/// on a variable that is not set, which would make a scan of somebody else's
/// config report errors for entries that are perfectly fine to use once the
/// variable is exported. Placeholders are shown as written.
///
/// Nothing here executes anything: it reads a file and describes it.
pub fn list_entries(path: &Path) -> Result<Vec<DiscoveredEntry>, String> {
    let source = std::fs::read_to_string(path).map_err(|error| plain_io_error(&error))?;
    let document: Document =
        serde_json::from_str(&source).map_err(|error| format!("not valid JSON: {error}"))?;
    let mut found: Vec<DiscoveredEntry> = document
        .mcp_servers
        .iter()
        .chain(document.servers.iter())
        .map(|(name, entry)| DiscoveredEntry {
            name: name.clone(),
            transport: entry.declared_transport().to_string(),
            summary: entry.summary(),
        })
        .collect();
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found.dedup_by(|a, b| a.name == b.name);
    Ok(found)
}

/// An io error without the path repeated: the caller already prints it.
fn plain_io_error(error: &std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::NotFound => "no such file".to_string(),
        std::io::ErrorKind::PermissionDenied => "permission denied".to_string(),
        _ => error.to_string(),
    }
}

impl Entry {
    /// What the entry says it is, before any validation.
    fn declared_transport(&self) -> &'static str {
        let declared = self.kind.as_deref().or(self.transport.as_deref());
        match declared {
            Some(kind) if kind.eq_ignore_ascii_case("stdio") => "stdio",
            Some(_) => "http",
            None if self.command.is_some() => "stdio",
            None if self.url.is_some() => "http",
            None => "?",
        }
    }

    /// The command line or URL as written, for a scan listing.
    fn summary(&self) -> String {
        if let Some(command) = &self.command {
            let mut line = command.clone();
            for arg in &self.args {
                line.push(' ');
                line.push_str(arg);
            }
            return line;
        }
        self.url
            .clone()
            .unwrap_or_else(|| "(no command or url)".to_string())
    }
}

#[derive(Debug, Default, Deserialize)]
struct Document {
    #[serde(default, rename = "mcpServers")]
    mcp_servers: BTreeMap<String, Entry>,
    #[serde(default)]
    servers: BTreeMap<String, Entry>,
}

#[derive(Debug, Default, Deserialize)]
struct Entry {
    #[serde(rename = "type")]
    kind: Option<String>,
    transport: Option<String>,
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    cwd: Option<String>,
    url: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ImportedTransport {
    Http,
    Stdio,
}

fn parse_document(
    source: &str,
    path: &Path,
    selected: &str,
    lookup: &impl Fn(&str) -> Option<String>,
) -> Result<ResolvedEntry, String> {
    let document: Document = serde_json::from_str(source)
        .map_err(|error| format!("{}: invalid MCP JSON config: {error}", path.display()))?;
    let mut entries = document.mcp_servers;
    for (name, entry) in document.servers {
        if entries.insert(name.clone(), entry).is_some() {
            return Err(format!(
                "{} defines server {name:?} in both `mcpServers` and `servers`",
                path.display()
            ));
        }
    }
    let entry = entries.get(selected).ok_or_else(|| {
        let available = entries.keys().cloned().collect::<Vec<_>>();
        if available.is_empty() {
            format!(
                "{} has no entries under `mcpServers` or `servers`",
                path.display()
            )
        } else {
            format!(
                "{} has no server named {selected:?}; available servers: {}",
                path.display(),
                available.join(", ")
            )
        }
    })?;
    resolve_entry(entry, path, selected, lookup)
}

fn resolve_entry(
    entry: &Entry,
    path: &Path,
    name: &str,
    lookup: &impl Fn(&str) -> Option<String>,
) -> Result<ResolvedEntry, String> {
    let workspace = workspace_folder(path);
    let declared = match (&entry.kind, &entry.transport) {
        (Some(kind), Some(transport)) => {
            let kind = parse_transport(kind)?;
            let transport = parse_transport(transport)?;
            if kind != transport {
                return Err(format!(
                    "server {name:?} has conflicting `type` and `transport` values"
                ));
            }
            Some(kind)
        }
        (Some(kind), None) | (None, Some(kind)) => Some(parse_transport(kind)?),
        (None, None) => None,
    };
    let has_command = entry.command.is_some();
    let has_url = entry.url.is_some();
    let transport = match (declared, has_command, has_url) {
        (Some(transport), _, _) => transport,
        (None, true, false) => ImportedTransport::Stdio,
        (None, false, true) => ImportedTransport::Http,
        (None, true, true) => {
            return Err(format!(
                "server {name:?} sets both `command` and `url`; add `type` to choose a transport"
            ));
        }
        (None, false, false) => {
            return Err(format!(
                "server {name:?} has neither `command` nor `url`, so its transport cannot be inferred"
            ));
        }
    };

    match transport {
        ImportedTransport::Stdio => {
            if entry.url.is_some() || !entry.headers.is_empty() {
                return Err(format!(
                    "stdio server {name:?} also sets HTTP-only `url` or `headers`"
                ));
            }
            let command = entry
                .command
                .as_deref()
                .ok_or_else(|| format!("stdio server {name:?} has no `command`"))?;
            let mut command_and_args = Vec::with_capacity(entry.args.len() + 1);
            command_and_args.push(expand(command, &workspace, lookup)?);
            for argument in &entry.args {
                command_and_args.push(expand(argument, &workspace, lookup)?);
            }
            if command_and_args[0].is_empty() {
                return Err(format!("stdio server {name:?} has an empty `command`"));
            }
            let env = entry
                .env
                .iter()
                .map(|(key, value)| {
                    expand(value, &workspace, lookup).map(|value| (key.clone(), value))
                })
                .collect::<Result<BTreeMap<_, _>, _>>()?;
            let cwd = entry
                .cwd
                .as_deref()
                .map(|cwd| expand(cwd, &workspace, lookup))
                .transpose()?
                .map(PathBuf::from)
                .map(|cwd| {
                    if cwd.is_absolute() {
                        cwd
                    } else {
                        workspace.join(cwd)
                    }
                });
            Ok(ResolvedEntry {
                connection: Connection::Stdio {
                    command: command_and_args,
                    env,
                    cwd,
                },
                http_trust: None,
            })
        }
        ImportedTransport::Http => {
            if entry.command.is_some()
                || !entry.args.is_empty()
                || !entry.env.is_empty()
                || entry.cwd.is_some()
            {
                return Err(format!(
                    "HTTP server {name:?} also sets stdio-only `command`, `args`, `env`, or `cwd`"
                ));
            }
            let url = entry
                .url
                .as_deref()
                .ok_or_else(|| format!("HTTP server {name:?} has no `url`"))?;
            let mut url_env_keys = BTreeSet::new();
            let url = expand_recording(url, &workspace, lookup, &mut url_env_keys)?;
            let mut header_env_keys = BTreeSet::new();
            let mut headers = Vec::with_capacity(entry.headers.len());
            for (key, value) in &entry.headers {
                headers.push((
                    key.clone(),
                    expand_recording(value, &workspace, lookup, &mut header_env_keys)?,
                ));
            }
            let mut header_names: Vec<String> = entry
                .headers
                .keys()
                .map(|name| name.to_ascii_lowercase())
                .collect();
            header_names.sort();
            header_names.dedup();
            Ok(ResolvedEntry {
                connection: Connection::Http {
                    url,
                    bearer: None,
                    headers,
                    oauth: None,
                },
                http_trust: Some(ImportedHttpTrust {
                    header_names,
                    header_env_keys: header_env_keys.into_iter().collect(),
                    url_env_keys: url_env_keys.into_iter().collect(),
                }),
            })
        }
    }
}

fn parse_transport(value: &str) -> Result<ImportedTransport, String> {
    match value.to_ascii_lowercase().replace(['-', '_'], "").as_str() {
        "stdio" => Ok(ImportedTransport::Stdio),
        "http" | "streamablehttp" => Ok(ImportedTransport::Http),
        "sse" => Err(
            "transport `sse` is not supported; mcp-repl requires Streamable HTTP (`http`)"
                .to_string(),
        ),
        _ => Err(format!(
            "unsupported imported transport {value:?}; expected `stdio` or `http`"
        )),
    }
}

fn workspace_folder(config_path: &Path) -> PathBuf {
    let parent = config_path.parent().unwrap_or_else(|| Path::new("."));
    if parent.file_name().is_some_and(|name| name == ".vscode") {
        parent.parent().unwrap_or(parent).to_path_buf()
    } else {
        parent.to_path_buf()
    }
}

fn expand(
    input: &str,
    workspace: &Path,
    lookup: &impl Fn(&str) -> Option<String>,
) -> Result<String, String> {
    expand_recording(input, workspace, lookup, &mut BTreeSet::new())
}

fn expand_recording(
    input: &str,
    workspace: &Path,
    lookup: &impl Fn(&str) -> Option<String>,
    env_keys: &mut BTreeSet<String>,
) -> Result<String, String> {
    let mut rendered = String::new();
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        rendered.push_str(&rest[..start]);
        let after_open = &rest[start + 2..];
        let Some(end) = after_open.find('}') else {
            return Err("unterminated `${...}` substitution in imported config".to_string());
        };
        let variable = &after_open[..end];
        let replacement = match variable {
            "workspaceFolder" => workspace.to_string_lossy().into_owned(),
            "workspaceFolderBasename" => workspace
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            "userHome" => {
                if let Some(value) = lookup("HOME") {
                    env_keys.insert("HOME".to_string());
                    value
                } else if let Some(value) = lookup("USERPROFILE") {
                    env_keys.insert("USERPROFILE".to_string());
                    value
                } else {
                    return Err(
                        "`${userHome}` requires the HOME or USERPROFILE environment variable"
                            .to_string(),
                    );
                }
            }
            variable if variable.starts_with("input:") => {
                return Err(format!(
                    "`${{{variable}}}` requires interactive client input, which mcp-repl cannot import; use an environment variable instead"
                ));
            }
            variable => {
                let variable = variable.strip_prefix("env:").unwrap_or(variable);
                if variable.is_empty() {
                    return Err(
                        "imported config contains an empty environment substitution".to_string()
                    );
                }
                env_keys.insert(variable.to_string());
                lookup(variable).ok_or_else(|| {
                    format!("imported config requires environment variable {variable:?}, but it is unset")
                })?
            }
        };
        rendered.push_str(&replacement);
        rest = &after_open[end + 1..];
    }
    rendered.push_str(rest);
    Ok(rendered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directories::{Directories, Platform};
    use crate::property::{
        GENERATED_CASES, Generator, INTERPOLATION_REGRESSIONS, SELECTOR_REGRESSIONS,
    };
    use std::ffi::OsString;

    #[test]
    fn property_selectors_and_interpolation_are_total_and_do_not_leak_on_error() {
        for selector in SELECTOR_REGRESSIONS {
            if let Some(Err(error)) = parse_selector(selector) {
                assert!(!error.is_empty());
            }
        }
        for input in INTERPOLATION_REGRESSIONS {
            let result = expand(input, Path::new("/workspace/project"), &|name| {
                Some(format!("value-for-{name}"))
            });
            if let Err(error) = result {
                assert!(!error.is_empty());
            }
        }

        let mut generator = Generator::new(0x03);
        for case in 0..GENERATED_CASES {
            let arbitrary = generator.text(128);
            if let Some(result) = parse_selector(&arbitrary) {
                match result {
                    Ok(selector) => {
                        assert!(!selector.path.as_os_str().is_empty());
                        assert!(!selector.entry.is_empty());
                    }
                    Err(error) => assert!(!error.is_empty()),
                }
            }

            let entry = format!("server_{case}");
            let selector = parse_selector(&format!("fuzz-{case}.JSON:{entry}"))
                .expect("a JSON suffix is an explicit selector")
                .expect("a non-empty selector is valid");
            assert_eq!(selector.entry, entry);

            let _ = expand(&arbitrary, Path::new("/workspace/project"), &|name| {
                Some(format!("value-for-{name}"))
            });

            let seeded_secret = format!("never-echo-{case:04x}");
            let error = expand(
                &format!("{seeded_secret}-${{env:MISSING}}"),
                Path::new("/workspace/project"),
                &|_| None,
            )
            .expect_err("the generated missing variable must be rejected");
            assert!(
                !error.contains(&seeded_secret),
                "secret leaked in {error:?}"
            );
        }
    }

    fn write(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn listing_describes_both_roots_and_both_transports() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            ".mcp.json",
            r#"{
                "mcpServers": {
                    "local": {"command": "node", "args": ["server.js", "--stdio"]}
                },
                "servers": {
                    "remote": {"type": "http", "url": "https://example.com/mcp"}
                }
            }"#,
        );
        let entries = list_entries(&path).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "local");
        assert_eq!(entries[0].transport, "stdio");
        assert_eq!(entries[0].summary, "node server.js --stdio");
        assert_eq!(entries[1].name, "remote");
        assert_eq!(entries[1].transport, "http");
        assert_eq!(entries[1].summary, "https://example.com/mcp");
    }

    #[test]
    fn listing_does_not_resolve_placeholders() {
        // Resolution fails on a variable that is not set. A scan of someone
        // else's config must still describe the entry, since exporting the
        // variable is all that is needed to use it.
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            ".mcp.json",
            r#"{"mcpServers": {"x": {"url": "${env:NEVER_SET_ANYWHERE}"}}}"#,
        );
        let entries = list_entries(&path).unwrap();
        assert_eq!(entries[0].summary, "${env:NEVER_SET_ANYWHERE}");
        // The resolving path is the one that objects.
        assert!(
            load_with(
                Selector {
                    path,
                    entry: "x".to_string()
                },
                |_| None
            )
            .is_err()
        );
    }

    #[test]
    fn a_malformed_file_reports_rather_than_disappearing() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), ".mcp.json", "not json {{{");
        let error = list_entries(&path).unwrap_err();
        assert!(error.contains("not valid JSON"), "{error}");

        // And a scan includes it, so a broken config is visible instead of
        // looking like an empty one.
        let scanned = scan(std::slice::from_ref(&path));
        assert_eq!(scanned.len(), 1);
        assert!(scanned[0].result.is_err());
    }

    #[test]
    fn scanning_skips_paths_that_do_not_exist() {
        let dir = tempfile::tempdir().unwrap();
        let real = write(
            dir.path(),
            ".mcp.json",
            r#"{"mcpServers":{"a":{"url":"http://x"}}}"#,
        );
        let scanned = scan(&[dir.path().join("absent.json"), real]);
        assert_eq!(scanned.len(), 1, "a missing file is not an error to report");
    }

    #[test]
    fn candidates_cover_the_project_files_and_the_home_ones() {
        let cwd = std::path::Path::new("/work/api");
        let home = std::path::Path::new("/home/dev");
        let directories = Directories::from_lookup(Platform::Unix, |name| {
            (name == "HOME").then(|| home.as_os_str().to_owned())
        });
        let paths = candidate_paths_with(cwd, &directories);
        for expected in [
            cwd.join(".mcp.json"),
            cwd.join(".vscode").join("mcp.json"),
            cwd.join(".cursor").join("mcp.json"),
            home.join(".claude.json"),
        ] {
            assert!(paths.contains(&expected), "missing {}", expected.display());
        }
        // Project files come first: a scan run in a repo means that repo.
        assert!(paths[0].starts_with(cwd));
        // Without a home directory it still offers the project files.
        let no_home = Directories::from_lookup(Platform::Unix, |_| None);
        assert_eq!(candidate_paths_with(cwd, &no_home).len(), 3);
    }

    #[test]
    fn windows_directories_find_appdata_candidates_without_a_home() {
        let cwd = std::path::Path::new("workspace");
        let directories = Directories::from_lookup(Platform::Windows, |name| {
            (name == "APPDATA").then(|| OsString::from(r"C:\Users\Ada\AppData\Roaming"))
        });
        let paths = candidate_paths_with(cwd, &directories);
        assert!(
            paths.contains(
                &PathBuf::from(r"C:\Users\Ada\AppData\Roaming")
                    .join("Claude")
                    .join("claude_desktop_config.json")
            ),
            "{paths:?}"
        );
        assert_eq!(paths.len(), 4, "only project and APPDATA paths are known");
    }

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let values: BTreeMap<String, String> = pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        move |key| values.get(key).cloned()
    }

    #[test]
    fn parses_claude_stdio_shape_with_workspace_and_environment() {
        let source = r#"{
          "mcpServers": {
            "local": {
              "command": "${workspaceFolder}/bin/server",
              "args": ["--repo", "${workspaceFolderBasename}"],
              "env": {"API_TOKEN": "${env:HOST_TOKEN}"},
              "cwd": "work"
            }
          }
        }"#;
        let resolved = parse_document(
            source,
            Path::new("/repo/.mcp.json"),
            "local",
            &env(&[("HOST_TOKEN", "secret")]),
        )
        .unwrap();
        assert_eq!(
            resolved.connection,
            Connection::Stdio {
                command: vec![
                    "/repo/bin/server".to_string(),
                    "--repo".to_string(),
                    "repo".to_string(),
                ],
                env: BTreeMap::from([("API_TOKEN".to_string(), "secret".to_string())]),
                cwd: Some(PathBuf::from("/repo/work")),
            }
        );
    }

    #[test]
    fn parses_vscode_http_shape_and_uses_workspace_parent() {
        let source = r#"{
          "servers": {
            "remote": {
              "type": "streamable-http",
              "url": "${env:MCP_URL}",
              "headers": {"Authorization": "Bearer ${TOKEN}"}
            }
          }
        }"#;
        let resolved = parse_document(
            source,
            Path::new("/repo/.vscode/mcp.json"),
            "remote",
            &env(&[("MCP_URL", "https://example/mcp"), ("TOKEN", "secret")]),
        )
        .unwrap();
        assert_eq!(
            resolved.connection,
            Connection::Http {
                url: "https://example/mcp".to_string(),
                bearer: None,
                headers: vec![("Authorization".to_string(), "Bearer secret".to_string())],
                oauth: None,
            }
        );
        assert_eq!(
            resolved.http_trust,
            Some(ImportedHttpTrust {
                header_names: vec!["authorization".to_string()],
                header_env_keys: vec!["TOKEN".to_string()],
                url_env_keys: vec!["MCP_URL".to_string()],
            })
        );
    }

    #[test]
    fn missing_entry_lists_sorted_names() {
        let error = parse_document(
            r#"{"mcpServers":{"z":{"command":"z"},"a":{"command":"a"}}}"#,
            Path::new("/repo/.mcp.json"),
            "missing",
            &env(&[]),
        )
        .unwrap_err();
        assert!(error.contains("a, z"), "{error}");
    }

    #[test]
    fn rejects_ambiguous_and_unsupported_transports() {
        let ambiguous = parse_document(
            r#"{"mcpServers":{"x":{"command":"x","url":"https://example"}}}"#,
            Path::new("/repo/.mcp.json"),
            "x",
            &env(&[]),
        )
        .unwrap_err();
        assert!(ambiguous.contains("both"), "{ambiguous}");

        let sse = parse_document(
            r#"{"servers":{"x":{"type":"sse","url":"https://example"}}}"#,
            Path::new("/repo/mcp.json"),
            "x",
            &env(&[]),
        )
        .unwrap_err();
        assert!(sse.contains("Streamable HTTP"), "{sse}");
    }

    #[test]
    fn missing_substitutions_name_the_variable_without_leaking_values() {
        let error = parse_document(
            r#"{
              "mcpServers": {
                "x": {
                  "command": "server",
                  "args": ["${env:MISSING}"],
                  "env": {"LITERAL_SECRET": "do-not-print-me"}
                }
              }
            }"#,
            Path::new("/repo/.mcp.json"),
            "x",
            &env(&[]),
        )
        .unwrap_err();
        assert!(error.contains("MISSING"), "{error}");
        assert!(!error.contains("do-not-print-me"), "{error}");
    }

    #[test]
    fn interactive_input_substitutions_are_actionable_errors() {
        let error = expand("${input:token}", Path::new("/repo"), &env(&[])).unwrap_err();
        assert!(error.contains("interactive"), "{error}");
        assert!(error.contains("environment variable"), "{error}");
    }

    #[test]
    fn selector_recognition_does_not_steal_ordinary_commands() {
        assert!(parse_selector("registry:serve").is_none());
        assert_eq!(
            parse_selector("path/to/.mcp.json:server").unwrap().unwrap(),
            Selector {
                path: PathBuf::from("path/to/.mcp.json"),
                entry: "server".to_string(),
            }
        );
    }
}
