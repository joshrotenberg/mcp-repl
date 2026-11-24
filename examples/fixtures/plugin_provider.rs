//! Minimal provider CLI double. Paths and state are confined to the test profile.
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

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
    let profile = profile_override
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = PathBuf::from(std::env::var_os("HOME").unwrap());
            home.join(if claude { ".claude" } else { ".codex" })
        });
    let test_root = PathBuf::from(std::env::var_os("MCP_REPL_PLUGIN_FIXTURE").unwrap());
    assert!(profile.starts_with(&test_root));
    assert_ne!(std::env::current_dir()?, test_root);
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
    let root = fs::read_to_string(&registered).ok().map(PathBuf::from);
    let version = fs::read_to_string(&installed).ok();
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
            assert!(Path::new(path).starts_with(&test_root));
            fs::write(registered, path)?;
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
            let manifest: Value = serde_json::from_slice(&fs::read(source.join("plugin.json"))?)?;
            let next = manifest["version"].as_str().unwrap();
            if let Some(previous) = version.as_deref().filter(|previous| *previous != next) {
                if claude {
                    mark_orphan(&cache(previous))?;
                } else {
                    fs::remove_dir_all(cache(previous))?;
                }
            }
            let path = cache(next);
            copy_tree(&source, &path)?;
            fs::write(installed, next)?;
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

fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn mark_orphan(cache: &Path) -> io::Result<()> {
    fs::write(cache.join(".orphaned_at"), "1795499690356")
}
