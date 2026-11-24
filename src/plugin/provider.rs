//! Provider CLI adapters. Provider output is used as evidence, never replayed.
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use super::{MARKETPLACE, PLUGIN_ID, Provider, Result, state};

const OUTPUT_LIMIT: usize = 1024 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub(super) struct Installation {
    pub(super) enabled: bool,
    pub(super) version: String,
    pub(super) path: Option<PathBuf>,
}

pub(super) struct Client<'a> {
    pub(super) provider: Provider,
    pub(super) cwd: &'a Path,
    pub(super) profile_override: Option<&'a Path>,
}

impl Client<'_> {
    async fn command(&self, args: &[&str], scoped: bool, path: Option<&Path>) -> Result<Value> {
        let exe = self.provider.program();
        let action = args[..if args[1] == "marketplace" { 3 } else { 2 }].join(" ");
        let failure = |reason: &str| {
            format!(
                "{exe} {action}: {reason}. {}",
                self.provider.inspection_hint()
            )
        };
        let mut command = Command::new(exe);
        command
            .current_dir(self.cwd)
            .env_remove("MCP_BEARER")
            .env("DISABLE_AUTOUPDATER", "1")
            .args(args);
        if let Some(profile) = self.profile_override {
            command.env(self.provider.profile_env(), profile);
        }
        if let Some(path) = path {
            command.arg(path);
        }
        if scoped && self.provider == Provider::Claude {
            command.args(["--scope", "user"]);
        }
        command
            .arg("--json")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|error| failure(&format!("could not launch executable: {error}")))?;
        let stdout = child.stdout.take().expect("piped provider stdout");
        let result = tokio::time::timeout(COMMAND_TIMEOUT, async {
            tokio::try_join!(child.wait(), limited_output(stdout))
        })
        .await;
        let (status, stdout) = match result {
            Ok(Ok(output)) => output,
            other => {
                // Kill and reap the immediate child. Descendants that outlive
                // their CLI parent are outside the provider process contract.
                let _ = child.start_kill();
                let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
                return Err(match other {
                    Err(_) => failure("command timed out"),
                    Ok(Err(error)) => failure(&format!("could not read command result: {error}")),
                    Ok(Ok(_)) => unreachable!(),
                });
            }
        };
        if !status.success() {
            return Err(failure(&format!("command exited with {status}")));
        }
        let result: Value = serde_json::from_slice(&stdout)
            .or_else(|error| {
                // Claude documents one result line at the end of mutation
                // output. Listings are ordinary JSON.
                if scoped && self.provider == Provider::Claude {
                    let last = stdout
                        .split(|byte| *byte == b'\n')
                        .rev()
                        .find(|line| line.iter().any(|byte| !byte.is_ascii_whitespace()))
                        .unwrap_or_default();
                    serde_json::from_slice(last)
                } else {
                    Err(error)
                }
            })
            .map_err(|_| failure("command did not return a supported JSON result"))?;
        if scoped && self.provider == Provider::Claude && result["outcome"] != "ok" {
            return Err(failure(
                "Claude Code did not confirm that the operation succeeded",
            ));
        }
        Ok(result)
    }

    pub(super) async fn marketplace(&self, expected: &Path) -> Result<bool> {
        let value = self
            .command(&["plugin", "marketplace", "list"], false, None)
            .await?;
        marketplace_present(self.provider, &value, expected)
    }

    pub(super) async fn installation(&self, source: &Path) -> Result<Option<Installation>> {
        let args: &[_] = match self.provider {
            Provider::Claude => &["plugin", "list"],
            Provider::Codex => &["plugin", "list", "--marketplace", MARKETPLACE],
        };
        let value = self.command(args, false, None).await?;
        installation(self.provider, &value, source)
    }

    pub(super) async fn register(&self, root: &Path) -> Result<()> {
        let value = self
            .command(&["plugin", "marketplace", "add"], true, Some(root))
            .await?;
        if self.provider == Provider::Codex
            && (value["marketplaceName"] != MARKETPLACE
                || value["alreadyAdded"] != false
                || !path_matches(&value["installedRoot"], root))
        {
            return Err("Codex did not confirm a new marketplace registration".into());
        }
        Ok(())
    }

    pub(super) async fn put(&self, update: bool) -> Result<Option<PathBuf>> {
        let verb = match self.provider {
            Provider::Claude if update => "update",
            Provider::Claude => "install",
            Provider::Codex => "add",
        };
        let value = self
            .command(&["plugin", verb, PLUGIN_ID], true, None)
            .await?;
        match self.provider {
            Provider::Claude => Ok(None),
            Provider::Codex if value["pluginId"] == PLUGIN_ID => value["installedPath"]
                .as_str()
                .map(|path| Some(PathBuf::from(path)))
                .ok_or_else(|| "Codex did not report the installed plugin path".into()),
            Provider::Codex => {
                Err("Codex did not confirm installation of the bundled plugin".into())
            }
        }
    }

    pub(super) async fn uninstall(&self) -> Result<()> {
        let args: &[_] = match self.provider {
            Provider::Claude => &["plugin", "uninstall", PLUGIN_ID, "--keep-data"],
            Provider::Codex => &["plugin", "remove", PLUGIN_ID],
        };
        self.command(args, true, None).await?;
        Ok(())
    }

    pub(super) async fn unregister(&self) -> Result<()> {
        self.command(
            &["plugin", "marketplace", "remove", MARKETPLACE],
            true,
            None,
        )
        .await?;
        Ok(())
    }
}

async fn limited_output(stdout: impl AsyncRead + Unpin) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    stdout
        .take((OUTPUT_LIMIT + 1) as u64)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() > OUTPUT_LIMIT {
        return Err(std::io::Error::other("JSON output exceeded 1 MiB"));
    }
    Ok(bytes)
}

fn marketplace_present(provider: Provider, value: &Value, expected: &Path) -> Result<bool> {
    let rows = match provider {
        Provider::Claude => value.as_array(),
        Provider::Codex => value["marketplaces"].as_array(),
    }
    .ok_or_else(|| "unsupported provider marketplace list response".to_string())?;
    let mut found = false;
    for row in rows {
        let name = row["name"]
            .as_str()
            .ok_or("marketplace list entry has no name")?;
        if name != MARKETPLACE {
            continue;
        }
        let same_root = match provider {
            Provider::Claude => {
                row["source"] == "directory" && path_matches(&row["path"], expected)
            }
            Provider::Codex => {
                row["marketplaceSource"]["sourceType"] == "local"
                    && path_matches(&row["root"], expected)
                    && path_matches(&row["marketplaceSource"]["source"], expected)
            }
        };
        if found || !same_root {
            return Err(format!("foreign or ambiguous marketplace {MARKETPLACE}"));
        }
        found = true;
    }
    Ok(found)
}

fn installation(provider: Provider, value: &Value, source: &Path) -> Result<Option<Installation>> {
    let rows = match provider {
        Provider::Claude => value.as_array(),
        Provider::Codex => value["installed"].as_array(),
    }
    .ok_or_else(|| "unsupported provider plugin list response".to_string())?;
    let id = if provider == Provider::Claude {
        "id"
    } else {
        "pluginId"
    };
    let mut installed = None;
    for row in rows {
        if row[id].as_str().ok_or("plugin list entry has no ID")? != PLUGIN_ID {
            continue;
        }
        if installed.is_some() || (provider == Provider::Claude && row["scope"] != "user") {
            return Err(format!("foreign or ambiguous installation of {PLUGIN_ID}"));
        }
        let expected = source.join("plugins").join("mcp-repl");
        let valid_source = match provider {
            Provider::Claude => path_matches(&row["readFromFolder"], &expected),
            Provider::Codex => {
                row["installed"] == true
                    && row["source"]["source"] == "local"
                    && path_matches(&row["source"]["path"], &expected)
                    && row["marketplaceSource"]["sourceType"] == "local"
                    && path_matches(&row["marketplaceSource"]["source"], source)
            }
        };
        if !valid_source {
            return Err(format!("provider reports a foreign source for {PLUGIN_ID}"));
        }
        let version = row["version"]
            .as_str()
            .ok_or("provider did not report the plugin version")?
            .to_owned();
        let enabled = row["enabled"]
            .as_bool()
            .ok_or("provider did not report whether the plugin is enabled")?;
        let path = if provider == Provider::Claude {
            Some(PathBuf::from(row["installPath"].as_str().ok_or(
                "Claude Code did not report the installed plugin path",
            )?))
        } else {
            None
        };
        installed = Some(Installation {
            enabled,
            path,
            version,
        });
    }
    Ok(installed)
}

fn path_matches(value: &Value, expected: &Path) -> bool {
    value
        .as_str()
        .is_some_and(|path| state::provider_path(Path::new(path)) == expected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn provider_json_distinguishes_same_name_foreign_marketplaces() {
        let root = Path::new("/managed/market");
        let claude = json!([{ "name": MARKETPLACE, "source": "directory", "path": root }]);
        assert!(marketplace_present(Provider::Claude, &claude, root).unwrap());
        assert!(marketplace_present(Provider::Claude, &claude, Path::new("/elsewhere")).is_err());
        let codex = json!({"marketplaces": [{
            "name": MARKETPLACE, "root": root,
            "marketplaceSource": {"sourceType": "local", "source": root}
        }]});
        assert!(marketplace_present(Provider::Codex, &codex, root).unwrap());
        let foreign = json!({"marketplaces": [{
            "name": MARKETPLACE, "root": root,
            "marketplaceSource": {"sourceType": "git", "source": root}
        }]});
        assert!(marketplace_present(Provider::Codex, &foreign, root).is_err());
    }

    #[test]
    fn codex_listing_verifies_source_without_requiring_an_installed_path() {
        let root = Path::new("/managed/market");
        let mut codex = json!({"installed": [{
            "pluginId": PLUGIN_ID, "installed": true, "enabled": false, "version": "0.4.1+bundle.1",
            "source": {"source": "local", "path": root.join("plugins/mcp-repl")},
            "marketplaceSource": {"sourceType": "local", "source": root}
        }]});
        let entry = installation(Provider::Codex, &codex, root)
            .unwrap()
            .unwrap();
        assert!(!entry.enabled);
        assert!(entry.path.is_none());
        codex["installed"][0]["source"]["path"] = json!("/foreign");
        assert!(installation(Provider::Codex, &codex, root).is_err());
    }
}
