//! Minimal provider CLI double. Paths and state are confined to the test profile.
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde_json::json;

const MARKET: &str = "mcp-repl-bundled";
const ID: &str = "mcp-repl@mcp-repl-bundled";

pub fn run(claude: bool) -> Result<(), tower_mcp::BoxError> {
    assert!(std::env::var_os("MCP_BEARER").is_none());
    assert_eq!(io::stdin().read(&mut [0])?, 0);
    assert_eq!(std::env::var("DISABLE_AUTOUPDATER")?, "1");

    let profile_var = if claude {
        "CLAUDE_CONFIG_DIR"
    } else {
        "CODEX_HOME"
    };
    let profile_override = std::env::var_os(profile_var);
    if claude
        && let Ok("unset" | "empty") =
            std::env::var("MCP_REPL_PLUGIN_FIXTURE_CLAUDE_DEFAULT").as_deref()
    {
        assert!(profile_override.is_none());
    }
    // The harness copies this executable into <owned root>/bin. Environment
    // values may select a known profile, but never supply filesystem roots.
    let executable = std::env::current_exe()?;
    let test_root = executable
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .canonicalize()?;
    // This double only runs below the harness's fixed workspace. Reject
    // parent traversal lexically as well as resolving its filesystem identity.
    let root_text = test_root.to_str().ok_or("non-UTF-8 fixture root")?;
    if root_text.contains("..") || test_root.file_name() != Some("plugin workspace".as_ref()) {
        return Err("invalid fixture root".into());
    }
    let test_root = PathBuf::from(root_text);
    assert_eq!(
        PathBuf::from(std::env::var_os("MCP_REPL_PLUGIN_FIXTURE").unwrap()).canonicalize()?,
        test_root
    );
    for variable in ["HOME", "USERPROFILE"] {
        assert_eq!(
            PathBuf::from(std::env::var_os(variable).ok_or("missing fixture home")?)
                .canonicalize()?,
            test_root
        );
    }
    let selected = profile_override
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| test_root.join(if claude { ".claude" } else { ".codex" }));
    let candidates: &[&str] = if claude {
        &["claude-profile", ".claude"]
    } else {
        &[
            "codex-profile",
            "another-codex",
            ".codex",
            "deleted-profile",
        ]
    };
    let mut args: Vec<_> = std::env::args().skip(1).collect();
    assert_eq!(args.pop().as_deref(), Some("--json"));
    let trace = if claude {
        "claude-trace"
    } else {
        "codex-trace"
    };
    use std::io::Write;
    writeln!(
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(test_root.join(trace))?,
        "{}",
        args.join(" ")
    )?;
    let profile = candidates
        .iter()
        .map(|name| test_root.join(name))
        .find(|candidate| {
            candidate
                .canonicalize()
                .ok()
                .is_some_and(|path| selected.canonicalize().ok().as_ref() == Some(&path))
        })
        .ok_or("unknown fixture profile")?;
    let canonical_root = test_root.canonicalize()?;
    assert!(profile.canonicalize()?.starts_with(&canonical_root));
    assert_ne!(std::env::current_dir()?.canonicalize()?, canonical_root);
    if !claude && std::env::var_os("MCP_REPL_PLUGIN_FIXTURE_FAIL_CODEX").is_some() {
        return Err("synthetic Codex failure".into());
    }
    if claude && !matches!(args.last().map(String::as_str), Some("list")) {
        assert_eq!(args.split_off(args.len() - 2), ["--scope", "user"]);
    }

    assert!(
        profile.is_dir(),
        "provider requires an existing profile directory"
    );
    let registered = profile.join("registered");
    let installed = profile.join("installed");
    let state_token = read_text(&registered).ok();
    let provider = if claude { "claude" } else { "codex" };
    let source_for = |token: &str| -> Result<PathBuf, tower_mcp::BoxError> {
        let state = match token {
            "state" => "state",
            "default-state" => "default-state",
            "deleted-profile-state" => "deleted-profile-state",
            _ => return Err("unknown fixture state".into()),
        };
        Ok(test_root
            .join(state)
            .join("mcp-repl/plugins")
            .join(provider))
    };
    let root = state_token.as_deref().map(source_for).transpose()?;
    let version = read_text(&installed)
        .ok()
        .map(checked_version)
        .transpose()?;
    let cache = |version: &str| {
        let name = if claude {
            version.replace('+', "-")
        } else {
            version.to_owned()
        };
        profile
            .join("plugins/cache")
            .join(MARKET)
            .join("mcp-repl")
            .join(name)
    };
    let strings: Vec<_> = args.iter().map(String::as_str).collect();
    let value = match strings.as_slice() {
        ["plugin", "marketplace", "list"] => {
            let rows: Vec<_> = root.iter().map(|root| {
                if claude {
                    json!({"name": MARKET, "source": "directory", "path": root})
                } else {
                    json!({"name": MARKET, "root": root, "marketplaceSource": {"sourceType": "local", "source": root}})
                }
            }).collect();
            if claude {
                json!(rows)
            } else {
                json!({"marketplaces": rows})
            }
        }
        ["plugin", "marketplace", "add", path] => {
            assert!(root.is_none());
            let selected = Path::new(path).canonicalize()?;
            let token = ["state", "default-state", "deleted-profile-state"]
                .into_iter()
                .find(|token| {
                    source_for(token).unwrap().canonicalize().ok().as_ref() == Some(&selected)
                })
                .ok_or("unknown fixture marketplace")?;
            fs::write(registered, token)?;
            if claude {
                json!({"outcome":"ok", "marketplace": MARKET})
            } else {
                json!({"marketplaceName": MARKET, "alreadyAdded": false, "installedRoot": path})
            }
        }
        ["plugin", "marketplace", "remove", MARKET] => {
            fs::remove_file(registered)?;
            if claude {
                json!({"outcome": "ok"})
            } else {
                json!({"marketplaceName": MARKET})
            }
        }
        ["plugin", "list"] | ["plugin", "list", "--marketplace", MARKET] => {
            let rows: Vec<_> = version.iter().map(|version| {
                let root = root.as_ref().unwrap();
                let source = root.join("plugins/mcp-repl");
                if claude {
                    json!({"id": ID, "scope": "user", "enabled": true, "version": version,
                        "installPath": cache(version), "readFromFolder": source})
                } else {
                    json!({"pluginId": ID, "installed": true, "enabled": true, "version": version,
                        "source": {"source": "local", "path": source},
                        "marketplaceSource": {"sourceType": "local", "source": root}})
                }
            }).collect();
            if claude {
                json!(rows)
            } else {
                json!({"installed": rows, "available": []})
            }
        }
        ["plugin", "install" | "add" | "update", ID] => {
            let source = root.unwrap().join("plugins/mcp-repl");
            // This fixture accepts exactly the bundled manifest template, with
            // a validated version substituted. It need not deserialize arbitrary JSON.
            let manifest = read_text(&source.join("plugin.json"))?;
            let (prefix, suffix) = include_str!("../../src/plugin_assets/plugin.json")
                .split_once("@VERSION@")
                .unwrap();
            let next = checked_version(
                manifest
                    .strip_prefix(prefix)
                    .and_then(|text| text.strip_suffix(suffix))
                    .ok_or("unexpected fixture manifest")?
                    .to_owned(),
            )?;
            if let Some(previous) = version
                .as_deref()
                .filter(|previous| *previous != next.as_str())
            {
                if claude {
                    mark_orphan(&cache(previous))?;
                } else {
                    fs::remove_dir_all(cache(previous))?;
                }
            }
            let path = cache(&next);
            copy_bundle(&source, &path)?;
            fs::write(installed, &next)?;
            if std::env::var_os("MCP_REPL_PLUGIN_FIXTURE_FAIL_AFTER_INSTALL").is_some() {
                return Err("synthetic failure after applying install".into());
            }
            if claude {
                // Claude can print a notice before the final JSON line.
                println!("fixture notice");
                json!({"outcome":"ok", "pluginId": ID})
            } else {
                json!({"pluginId": ID, "installedPath": path})
            }
        }
        ["plugin", "uninstall", ID, "--keep-data"] | ["plugin", "remove", ID] => {
            fs::remove_file(installed)?;
            let path = cache(&version.unwrap());
            if claude {
                mark_orphan(&path)?;
                json!({"outcome": "ok", "keptData": true})
            } else {
                fs::remove_dir_all(path.parent().unwrap().parent().unwrap())?;
                json!({"pluginId": ID})
            }
        }
        _ => panic!("unexpected provider argv {args:?}"),
    };
    println!("{value}");
    Ok(())
}

// A fixture copies exactly the packaged plugin inventory, never arbitrary
// directory entry names supplied by a provider or receipt.
fn copy_bundle(from: &Path, to: &Path) -> io::Result<()> {
    for name in [
        "plugin.json",
        ".claude-plugin/plugin.json",
        "skills/mcp-repl/SKILL.md",
    ] {
        let target = to.join(name);
        fs::create_dir_all(target.parent().unwrap())?;
        fs::copy(from.join(name), target)?;
    }
    Ok(())
}

fn read_text(path: &Path) -> io::Result<String> {
    const LIMIT: u64 = 1024 * 1024;
    let mut text = String::new();
    fs::File::open(path)?
        .take(LIMIT + 1)
        .read_to_string(&mut text)?;
    if text.len() as u64 > LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "fixture input exceeds limit",
        ));
    }
    Ok(text)
}

fn checked_version(version: String) -> Result<String, tower_mcp::BoxError> {
    if version.is_empty()
        || version.len() > 128
        || version.contains("..")
        || !version.starts_with(|c: char| c.is_ascii_digit())
        || !version
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b".-+".contains(&c))
        || version.ends_with('.')
    {
        return Err("invalid fixture version".into());
    }
    Ok(version)
}

fn mark_orphan(cache: &Path) -> io::Result<()> {
    fs::write(cache.join(".orphaned_at"), "1795499690356")
}
