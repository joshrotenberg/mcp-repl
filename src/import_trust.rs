//! Approval for using a server named by an imported client config.
//!
//! A native profile is in the user's own config file. An imported `.mcp.json`
//! entry is not: it can arrive with a repository, choose a program to run, or
//! choose a remote HTTP origin and headers to send. The first time an entry is
//! used, the REPL shows the security-relevant, non-secret identity and asks.
//!
//! Approvals are recorded in `approved-imports.toml` beside the config,
//! keyed by what was approved rather than by a hash of it: the file is meant
//! to be readable, so an operator can audit what past runs agreed to, and
//! deleting it forgets everything. A changed command, working directory,
//! HTTP origin, header set, or set of forwarded variables does not match the
//! record, so it asks again.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use nu_ansi_term::{Color, Style};
use serde::{Deserialize, Serialize};

use crate::secure_file;
use crate::style::{paint, sanitize, tag};

/// What a selected import resolved to, and what approval is checked against.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportPlan {
    /// The config file the entry came from, absolute where it can be.
    pub source: String,
    /// The entry name inside that file.
    pub entry: String,
    /// The program and its arguments, exactly as they will be spawned.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// The working directory, when the entry sets one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// The normalized HTTP origin. Paths, queries, and credentials are never
    /// persisted in the approval store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// Header names an imported HTTP entry supplies. Values are deliberately
    /// absent for the same reason environment values are absent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub header_names: Vec<String>,
    /// Names of the variables the entry references for a child or HTTP
    /// connection. Values are deliberately absent: this file records what
    /// was approved, and a decision record is not a place to copy secrets.
    #[serde(default)]
    pub env_keys: Vec<String>,
}

impl ImportPlan {
    pub fn stdio(
        source: &Path,
        entry: &str,
        command: &[String],
        cwd: Option<&Path>,
        env: &BTreeMap<String, String>,
    ) -> Self {
        let mut env_keys: Vec<String> = env.keys().cloned().collect();
        env_keys.sort();
        Self {
            // Canonicalize so the same file reached by different relative
            // paths is one entry; fall back to what was given when the path
            // cannot be resolved.
            source: std::fs::canonicalize(source)
                .unwrap_or_else(|_| source.to_path_buf())
                .display()
                .to_string(),
            entry: entry.to_string(),
            command: command.to_vec(),
            cwd: cwd.map(|p| p.display().to_string()),
            origin: None,
            header_names: Vec::new(),
            env_keys,
        }
    }

    pub fn http(
        source: &Path,
        entry: &str,
        destination: &str,
        header_names: &[String],
        env_keys: &[String],
    ) -> Result<Self, String> {
        let url = url::Url::parse(destination)
            .map_err(|error| format!("invalid imported HTTP URL: {error}"))?;
        if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
            return Err(
                "invalid imported HTTP URL; expected an http:// or https:// URL".to_string(),
            );
        }
        let mut header_names: Vec<String> = header_names
            .iter()
            .map(|name| name.to_ascii_lowercase())
            .collect();
        header_names.sort();
        header_names.dedup();
        let mut env_keys = env_keys.to_vec();
        env_keys.sort();
        env_keys.dedup();
        Ok(Self {
            source: canonical_source(source),
            entry: entry.to_string(),
            command: Vec::new(),
            cwd: None,
            origin: Some(url.origin().ascii_serialization()),
            header_names,
            env_keys,
        })
    }

    /// The variables being forwarded whose names look like credentials.
    fn credential_env_keys(&self) -> Vec<&String> {
        self.env_keys
            .iter()
            .filter(|key| crate::wire::looks_like_credential(key))
            .collect()
    }

    /// The command as a single shell-ish line, for display only. Nothing
    /// re-parses this; the child is spawned from the argument vector.
    fn command_line(&self) -> String {
        self.command
            .iter()
            .map(|part| {
                if part.contains(char::is_whitespace) {
                    format!("{part:?}")
                } else {
                    part.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

fn canonical_source(source: &Path) -> String {
    std::fs::canonicalize(source)
        .unwrap_or_else(|_| source.to_path_buf())
        .display()
        .to_string()
}

/// The `approved-imports.toml` document.
#[derive(Debug, Default, Deserialize, Serialize)]
struct Approvals {
    #[serde(default, rename = "approved")]
    entries: Vec<ImportPlan>,
}

/// Where approvals live: beside the config file that named the profiles.
pub fn approvals_path(config_path: Option<&Path>) -> Option<PathBuf> {
    let directory = config_path?.parent()?;
    Some(directory.join("approved-imports.toml"))
}

fn load(path: &Path) -> Approvals {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Approvals::default();
    };
    // A corrupt or hand-mangled trust store is treated as empty: asking
    // again is the safe direction.
    toml::from_str(&text).unwrap_or_default()
}

fn remember(path: &Path, plan: &ImportPlan) -> Result<(), String> {
    let mut approvals = load(path);
    if !approvals.entries.iter().any(|known| known == plan) {
        approvals.entries.push(plan.clone());
    }
    let text = toml::to_string_pretty(&approvals)
        .map_err(|e| format!("could not encode {}: {e}", path.display()))?;
    let document = format!(
        "# Imported client-config entries approved for use, written by\n\
         # mcp-repl. Delete a block to be asked about it again, or delete the\n\
         # file to forget every approval.\n\n{text}"
    );
    secure_file::write_atomic(path, &document)
        .map_err(|e| format!("could not write {}: {e}", path.display()))
}

/// Whether this exact plan was approved before.
fn is_approved(path: Option<&Path>, plan: &ImportPlan) -> bool {
    path.is_some_and(|path| load(path).entries.iter().any(|known| known == plan))
}

/// The outcome of asking about a plan.
pub enum Decision {
    /// Use the imported entry.
    Approved,
    /// Do not use it, with the reason to report.
    Refused(String),
}

/// Decide whether to use an imported entry.
///
/// `trusted` is `--trust-import`, which approves without asking and without
/// recording. `interactive` is false under `--exec`/`--json` and when stdin
/// is not a terminal, where there is nobody to ask: rather than blocking on
/// a read that will never return, the run stops with the flag to pass.
pub fn authorize(
    plan: &ImportPlan,
    config_path: Option<&Path>,
    trusted: bool,
    interactive: bool,
) -> Decision {
    if trusted {
        tracing::debug!(source = %plan.source, entry = %plan.entry, "import approved by --trust-import");
        return Decision::Approved;
    }
    let store = approvals_path(config_path);
    if is_approved(store.as_deref(), plan) {
        tracing::debug!(
            source = %plan.source,
            entry = %plan.entry,
            store = ?store.as_deref().map(|p| p.display().to_string()),
            "import approved by a recorded approval"
        );
        return Decision::Approved;
    }
    tracing::debug!(
        source = %plan.source,
        entry = %plan.entry,
        interactive,
        "import has no recorded approval"
    );
    if !interactive {
        return Decision::Refused(format!(
            "refusing to use the server imported from {}:{} without approval.\n\
             This entry has not been approved before, and a non-interactive session has \
             nobody to ask. Run it once interactively to review and approve it, or pass \
             --trust-import to skip the check.",
            plan.source, plan.entry
        ));
    }
    describe(plan);
    match confirm() {
        false => Decision::Refused(format!(
            "not using {}:{}",
            plan.source,
            sanitize(&plan.entry)
        )),
        true => {
            match store {
                Some(path) => {
                    if let Err(e) = remember(&path, plan) {
                        // The import was approved for this run either way;
                        // only the memory of it failed.
                        eprintln!("warning: {e}");
                    }
                }
                None => eprintln!(
                    "note: no config location, so this approval is not saved and will be \
                     asked again next time"
                ),
            }
            Decision::Approved
        }
    }
}

/// Show what will run. Everything here comes from the imported file, so it
/// is sanitized before display like any other untrusted text.
fn describe(plan: &ImportPlan) {
    if let Some(origin) = &plan.origin {
        println!(
            "{} {}:{} wants to connect to an HTTP server:",
            tag(Style::new().fg(Color::Yellow), "import"),
            sanitize(&plan.source),
            sanitize(&plan.entry)
        );
        println!(
            "  origin:  {}",
            paint(Style::new().bold(), &sanitize(origin))
        );
        if !plan.header_names.is_empty() {
            let names: Vec<String> = plan
                .header_names
                .iter()
                .map(|name| sanitize(name).into_owned())
                .collect();
            println!("  headers: {}", names.join(", "));
        }
        if !plan.env_keys.is_empty() {
            let names: Vec<String> = plan
                .env_keys
                .iter()
                .map(|key| sanitize(key).into_owned())
                .collect();
            println!("  env:     {}", names.join(", "));
        }
        for name in &plan.header_names {
            if crate::wire::looks_like_credential(name) {
                println!(
                    "  {} {} can carry a credential to this origin",
                    paint(Style::new().fg(Color::Yellow).bold(), "warning:"),
                    sanitize(name)
                );
            }
        }
        for key in plan.credential_env_keys() {
            println!(
                "  {} {} looks like a credential, and its value can be sent to this origin",
                paint(Style::new().fg(Color::Yellow).bold(), "warning:"),
                sanitize(key)
            );
        }
        println!(
            "{}",
            paint(
                Style::new().dimmed(),
                "The imported file chooses the remote server and headers. Approve it only if you trust that file."
            )
        );
        return;
    }
    println!(
        "{} {}:{} wants to start a server process on this machine:",
        tag(Style::new().fg(Color::Yellow), "import"),
        sanitize(&plan.source),
        sanitize(&plan.entry)
    );
    println!(
        "  command: {}",
        paint(Style::new().bold(), &sanitize(&plan.command_line()))
    );
    if let Some(cwd) = &plan.cwd {
        println!("  cwd:     {}", sanitize(cwd));
    }
    if !plan.env_keys.is_empty() {
        let names: Vec<String> = plan
            .env_keys
            .iter()
            .map(|key| sanitize(key).into_owned())
            .collect();
        println!("  env:     {}", names.join(", "));
    }
    for key in plan.credential_env_keys() {
        println!(
            "  {} {} looks like a credential, and its value goes to this program",
            paint(Style::new().fg(Color::Yellow).bold(), "warning:"),
            sanitize(key)
        );
    }
    println!(
        "{}",
        paint(
            Style::new().dimmed(),
            "The imported file chooses the program and which of your environment variables \
             it receives. Approve it only if you trust that file."
        )
    );
}

fn confirm() -> bool {
    print!("  approve it? [y/N]> ");
    let _ = std::io::stdout().flush();
    let mut buf = String::new();
    let read = {
        let mut lock = std::io::stdin().lock();
        std::io::BufRead::read_line(&mut lock, &mut buf)
    };
    match read {
        Ok(0) | Err(_) => false,
        Ok(_) => matches!(buf.trim(), "y" | "Y" | "yes" | "Yes"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(command: &[&str]) -> ImportPlan {
        let mut env = BTreeMap::new();
        env.insert("API_TOKEN".to_string(), "secret-value".to_string());
        env.insert("REGION".to_string(), "us-east-1".to_string());
        ImportPlan::stdio(
            Path::new("/repo/.mcp.json"),
            "local",
            &command.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            Some(Path::new("/repo")),
            &env,
        )
    }

    #[test]
    fn a_recorded_plan_is_not_asked_about_again() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("approved-imports.toml");
        let plan = plan(&["server", "--stdio"]);
        assert!(!is_approved(Some(&store), &plan));
        remember(&store, &plan).unwrap();
        assert!(is_approved(Some(&store), &plan));
    }

    #[test]
    fn a_changed_command_is_a_different_plan() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("approved-imports.toml");
        remember(&store, &plan(&["server", "--stdio"])).unwrap();
        // The entry now runs something else: the old approval must not
        // cover it.
        assert!(!is_approved(Some(&store), &plan(&["curl", "evil.sh"])));
    }

    #[test]
    fn a_changed_env_set_is_a_different_plan() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("approved-imports.toml");
        let approved = plan(&["server"]);
        remember(&store, &approved).unwrap();
        let mut widened = approved.clone();
        widened.env_keys.push("AWS_SECRET_ACCESS_KEY".to_string());
        assert!(!is_approved(Some(&store), &widened));
    }

    #[test]
    fn the_store_never_holds_env_values() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("approved-imports.toml");
        remember(&store, &plan(&["server"])).unwrap();
        let written = std::fs::read_to_string(&store).unwrap();
        assert!(written.contains("API_TOKEN"), "keys are the record");
        assert!(
            !written.contains("secret-value"),
            "a trust record must not become a place secrets live"
        );
    }

    #[test]
    fn credential_shaped_variables_are_singled_out() {
        let mut env = BTreeMap::new();
        env.insert("GITHUB_TOKEN".to_string(), "x".to_string());
        env.insert("AWS_SECRET_ACCESS_KEY".to_string(), "x".to_string());
        env.insert("REGION".to_string(), "x".to_string());
        let plan = ImportPlan::stdio(Path::new("/repo/.mcp.json"), "local", &[], None, &env);
        let flagged: Vec<&str> = plan
            .credential_env_keys()
            .into_iter()
            .map(String::as_str)
            .collect();
        assert_eq!(flagged, vec!["AWS_SECRET_ACCESS_KEY", "GITHUB_TOKEN"]);
    }

    #[test]
    fn a_corrupt_store_asks_again_rather_than_trusting() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("approved-imports.toml");
        std::fs::write(&store, "this is not toml {{{").unwrap();
        assert!(!is_approved(Some(&store), &plan(&["server"])));
    }

    #[test]
    fn without_a_config_location_nothing_is_pre_approved() {
        assert!(!is_approved(None, &plan(&["server"])));
    }

    #[test]
    fn trust_import_skips_the_check_entirely() {
        assert!(matches!(
            authorize(&plan(&["server"]), None, true, false),
            Decision::Approved
        ));
    }

    #[test]
    fn a_non_interactive_session_refuses_with_guidance() {
        match authorize(&plan(&["server"]), None, false, false) {
            Decision::Refused(message) => {
                assert!(message.contains("--trust-import"));
                assert!(message.contains("local"));
            }
            Decision::Approved => panic!("an unapproved entry must not run unattended"),
        }
    }

    fn http_plan(destination: &str, headers: &[&str], env_keys: &[&str]) -> ImportPlan {
        ImportPlan::http(
            Path::new("/repo/.mcp.json"),
            "remote",
            destination,
            &headers
                .iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>(),
            &env_keys
                .iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>(),
        )
        .unwrap()
    }

    #[test]
    fn an_http_plan_records_only_the_origin_and_forwarded_names() {
        let plan = http_plan(
            "https://user:password@example.com:443/private?token=literal-secret",
            &["X-Api-Key", "Authorization"],
            &["TOKEN", "MCP_URL", "TOKEN"],
        );
        assert_eq!(plan.origin.as_deref(), Some("https://example.com"));
        assert_eq!(plan.header_names, ["authorization", "x-api-key"]);
        assert_eq!(plan.env_keys, ["MCP_URL", "TOKEN"]);

        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("approved-imports.toml");
        remember(&store, &plan).unwrap();
        let written = std::fs::read_to_string(store).unwrap();
        assert!(written.contains("https://example.com"));
        assert!(written.contains("authorization"));
        assert!(written.contains("TOKEN"));
        for secret in ["user", "password", "private", "literal-secret"] {
            assert!(!written.contains(secret), "approval leaked {secret:?}");
        }
    }

    #[test]
    fn an_http_origin_or_forwarded_set_change_requires_approval() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("approved-imports.toml");
        let approved = http_plan("https://api.example/mcp", &["Authorization"], &["TOKEN"]);
        remember(&store, &approved).unwrap();
        assert!(is_approved(
            Some(&store),
            &http_plan(
                "https://api.example/another-path",
                &["authorization"],
                &["TOKEN"]
            )
        ));
        assert!(!is_approved(
            Some(&store),
            &http_plan("https://other.example/mcp", &["Authorization"], &["TOKEN"])
        ));
        assert!(!is_approved(
            Some(&store),
            &http_plan(
                "https://api.example/mcp",
                &["Authorization", "X-Api-Key"],
                &["TOKEN"]
            )
        ));
        assert!(!is_approved(
            Some(&store),
            &http_plan(
                "https://api.example/mcp",
                &["Authorization"],
                &["TOKEN", "SECOND_TOKEN"]
            )
        ));
    }

    #[test]
    fn existing_stdio_approval_records_remain_readable() {
        let old = r#"
            [[approved]]
            source = "/repo/.mcp.json"
            entry = "local"
            command = ["server", "--stdio"]
            cwd = "/repo"
            env_keys = ["API_TOKEN", "REGION"]
        "#;
        let approvals: Approvals = toml::from_str(old).unwrap();
        assert_eq!(approvals.entries, vec![plan(&["server", "--stdio"])]);
    }
}
