//! Manage the bundled, local skills plugin through provider CLIs.
mod provider;
mod state;

use std::path::{Path, PathBuf};

use clap::{Arg, ArgAction, ArgMatches, Command, ValueEnum};
use serde::Serialize;

use crate::directories::Directories;
use provider::Client;
use state::{Receipt, State};

const MARKETPLACE: &str = "mcp-repl-bundled";
const PLUGIN_ID: &str = "mcp-repl@mcp-repl-bundled";
type Result<T> = std::result::Result<T, String>;

#[derive(Clone, Copy, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Provider {
    Claude,
    Codex,
}

impl Provider {
    fn program(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    fn profile_env(self) -> &'static str {
        match self {
            Self::Claude => "CLAUDE_CONFIG_DIR",
            Self::Codex => "CODEX_HOME",
        }
    }

    fn configured_profile(self) -> Option<PathBuf> {
        std::env::var_os(self.profile_env())
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    }

    fn default_profile(self, directories: &Directories) -> Result<PathBuf> {
        directories
            .home()
            .map(|home| home.join(format!(".{}", self.program())))
            .ok_or_else(|| format!("cannot locate the {} profile", self.program()))
    }

    fn inspection_hint(self) -> &'static str {
        match self {
            Self::Claude => {
                "Inspect the same profile with `claude plugin marketplace list --json` and `claude plugin list --json`."
            }
            Self::Codex => {
                "Inspect the same profile with `codex plugin marketplace list --json` and, if registered, `codex plugin list --marketplace mcp-repl-bundled --json`."
            }
        }
    }

    fn cache(self, profile: &Path) -> PathBuf {
        profile.join("plugins").join("cache").join(MARKETPLACE)
    }

    fn cache_version(self, profile: &Path, version: &str) -> PathBuf {
        let version = match self {
            Self::Claude => version.replace('+', "-"),
            Self::Codex => version.to_owned(),
        };
        self.cache(profile).join("mcp-repl").join(version)
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum Selection {
    Claude,
    Codex,
    Both,
}

impl Selection {
    fn providers(self) -> &'static [Provider] {
        match self {
            Self::Claude => &[Provider::Claude],
            Self::Codex => &[Provider::Codex],
            Self::Both => &[Provider::Claude, Provider::Codex],
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Operation {
    Install,
    Update,
    Uninstall,
    Status,
}

impl Operation {
    const ALL: [(Self, &'static str, &'static str); 4] = [
        (Self::Install, "install", "Install the bundled plugin"),
        (
            Self::Update,
            "update",
            "Update a managed installation to this executable's bundle",
        ),
        (
            Self::Uninstall,
            "uninstall",
            "Remove a managed installation and its local source",
        ),
        (
            Self::Status,
            "status",
            "Inspect local ownership and bundle files without running providers",
        ),
    ];

    fn name(self) -> &'static str {
        Self::ALL
            .iter()
            .find(|(operation, _, _)| *operation == self)
            .unwrap()
            .1
    }
}

pub(super) fn command() -> Command {
    let mut command = Command::new("plugin")
        .about("Manage the bundled mcp-repl skills for Claude Code and Codex")
        .disable_help_subcommand(true)
        .arg_required_else_help(true)
        .subcommand_required(true)
        .arg(
            Arg::new("provider")
                .long("provider")
                .global(true)
                .value_parser(clap::value_parser!(Selection))
                .help("Choose a provider (status defaults to both, mutations require a choice)"),
        )
        .arg(
            Arg::new("plugin-json")
                .long("json")
                .global(true)
                .action(ArgAction::SetTrue)
                .help("Emit one JSON report"),
        )
        .arg(
            Arg::new("dry-run")
                .long("dry-run")
                .global(true)
                .action(ArgAction::SetTrue)
                .help("Inspect local state without writes or provider commands"),
        );
    for (_, name, about) in Operation::ALL {
        command = command.subcommand(Command::new(name).about(about));
    }
    command
}

#[derive(Serialize)]
struct Report {
    operation: Operation,
    dry_run: bool,
    bundle_version: String,
    providers: Vec<ProviderReport>,
}

#[derive(Serialize)]
struct ProviderReport {
    provider: Provider,
    outcome: &'static str,
    message: String,
}

pub(super) fn run(matches: &ArgMatches, root_json: bool) -> crate::ExitStatus {
    let (name, action) = matches.subcommand().expect("required plugin action");
    let operation = Operation::ALL
        .iter()
        .find(|(_, verb, _)| *verb == name)
        .unwrap()
        .0;
    let selection = match action.get_one::<Selection>("provider") {
        Some(selection) => *selection,
        None if operation == Operation::Status => Selection::Both,
        None => crate::exit_with_error(
            crate::ExitStatus::Usage,
            "plugin mutations require --provider claude, codex, or both",
        ),
    };
    let dry_run = action.get_flag("dry-run");
    let version = state::bundle_version();
    let directories = Directories::current();
    let base = directories
        .state_home()
        .ok_or_else(|| "cannot locate the mcp-repl state directory".to_owned())
        .and_then(state::resolve_root)
        .map(|path| path.join("mcp-repl").join("plugins"));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build plugin manager runtime");
    let mut providers = Vec::new();
    for &provider in selection.providers() {
        let (outcome, message) = match &base {
            Ok(base) => runtime.block_on(manage(
                &directories,
                base,
                provider,
                operation,
                dry_run,
                &version,
            )),
            Err(error) => Err(error.clone()),
        }
        .unwrap_or_else(|error| ("error", error));
        providers.push(ProviderReport {
            provider,
            outcome,
            message,
        });
    }
    let report = Report {
        operation,
        dry_run,
        bundle_version: version,
        providers,
    };
    if root_json || action.get_flag("plugin-json") {
        println!(
            "{}",
            serde_json::to_string(&report).expect("serialize plugin report")
        );
    } else {
        for row in &report.providers {
            println!(
                "{}: {} - {}",
                row.provider.program(),
                row.outcome,
                row.message
            );
        }
    }
    if report
        .providers
        .iter()
        .any(|row| matches!(row.outcome, "error" | "pending"))
    {
        crate::ExitStatus::Server
    } else {
        crate::ExitStatus::Success
    }
}

async fn manage(
    directories: &Directories,
    base: &Path,
    provider: Provider,
    operation: Operation,
    dry_run: bool,
    version: &str,
) -> Result<(&'static str, String)> {
    let configured = provider.configured_profile();
    let logical = match &configured {
        Some(path) => state::provider_path(path).into_owned(),
        None => state::provider_path(&provider.default_profile(directories)?).into_owned(),
    };
    let profile = state::resolve_root(&logical)?;
    // Marketplace source must stay separate from provider settings and caches.
    if base.starts_with(&profile) || profile.starts_with(base) {
        return Err("provider profile overlaps the mcp-repl plugin state directory".into());
    }
    let state = State::new(base, provider);
    let offline = dry_run || operation == Operation::Status;
    let _lock = state.lock(!offline)?;
    let receipt = state.read(&profile)?;
    if let Some(pending) = receipt
        .as_ref()
        .and_then(|record| record.pending.as_deref())
    {
        let message = state.pending_message(pending, provider);
        return if offline {
            Ok(("pending", message))
        } else {
            Err(message)
        };
    }
    if offline {
        verify_cache(provider, &profile, receipt.as_ref())?;
        let active = receipt.as_ref().filter(|receipt| receipt.active());
        let local = match active {
            None => "no managed installation".into(),
            Some(receipt) if receipt.version == version => {
                format!(
                    "recorded bundle {} matches this executable",
                    receipt.version
                )
            }
            Some(receipt) => format!(
                "recorded bundle {} differs from this executable",
                receipt.version
            ),
        };
        let message = if dry_run && operation != Operation::Status {
            format!(
                "{}: {local}. A real run checks provider registration and installation before applying changes.",
                operation.name()
            )
        } else {
            format!("{local}. Provider settings were not queried.")
        };
        return Ok((
            if dry_run {
                "dry-run"
            } else if active.is_some() {
                "managed"
            } else {
                "unmanaged"
            },
            message,
        ));
    }

    verify_cache(provider, &profile, receipt.as_ref())?;
    if state::metadata(&profile)?.is_none() {
        match operation {
            Operation::Install => state::initialize_profile(&profile)?,
            Operation::Uninstall => {
                return Ok(("unchanged", "no managed plugin to uninstall".into()));
            }
            _ => {
                return Err(format!(
                    "provider profile {} does not exist; run plugin install first",
                    profile.display()
                ));
            }
        }
    }
    let active = receipt.as_ref().is_some_and(Receipt::active);
    let cwd = tempfile::tempdir()
        .map_err(|error| format!("could not create neutral provider working directory: {error}"))?;
    let client = Client {
        provider,
        cwd: cwd.path(),
        // Claude uses different global state with a nonempty override.
        profile_override: (configured.is_some() || provider == Provider::Codex)
            .then_some(profile.as_path()),
    };
    let registered = client.marketplace(&state.source).await?;
    if registered != active {
        return Err(
            "marketplace registration does not match the local receipt; inspect provider settings"
                .into(),
        );
    }
    // Codex's marketplace-filtered listing requires the marketplace to exist.
    let installed = if registered || provider == Provider::Claude {
        client.installation(&state.source).await?
    } else {
        None
    };
    if installed.is_some() != active {
        return Err(
            "provider installation does not match the local receipt; inspect provider settings"
                .into(),
        );
    }
    if let (Some(record), Some(installation)) = (&receipt, &installed) {
        check_installation(provider, &profile, &logical, installation, &record.version)?;
    }

    let mut record = match (operation, active, receipt) {
        (Operation::Uninstall, false, _) => {
            return Ok(("unchanged", "no managed plugin to uninstall".into()));
        }
        (Operation::Uninstall, true, Some(mut record)) => {
            record.pending = Some("uninstall".into());
            state.save(&record)?;
            client.uninstall().await?;
            if client.installation(&state.source).await?.is_some() {
                return Err("provider still reports the plugin after uninstall".into());
            }
            client.unregister().await?;
            if client.marketplace(&state.source).await? {
                return Err("provider still reports the marketplace after removal".into());
            }
            state.remove(&record.files)?;
            // Keep ownership snapshots for Claude's retained cache versions.
            // The inactive receipt permits reinstall only after verification.
            record.version.clear();
            record.files.clear();
            prune_caches(provider, &profile, &mut record)?;
            record.pending = None;
            if record.caches.is_empty() {
                state.discard_receipt()?;
            } else {
                state.save(&record)?;
            }
            return Ok((
                "uninstalled",
                "removed the managed plugin and marketplace registration".into(),
            ));
        }
        (Operation::Update, false, _) => {
            return Err("no managed plugin to update; run plugin install first".into());
        }
        (Operation::Install, true, Some(record)) if record.version != version => {
            return Ok((
                "unchanged",
                "already managed; run plugin update to apply this executable's bundle".into(),
            ));
        }
        (_, true, Some(record)) if record.version == version => {
            return Ok((
                "unchanged",
                "already at this executable's bundle version".into(),
            ));
        }
        (Operation::Update, true, Some(_))
            if installed.as_ref().is_some_and(|entry| !entry.enabled) =>
        {
            return Err(
                "the managed plugin is disabled; updating could change its enabled state".into(),
            );
        }
        (_, _, Some(record)) => record,
        (_, _, None) => Receipt {
            schema: 1,
            profile: profile.clone(),
            files: Default::default(),
            version: String::new(),
            caches: Default::default(),
            pending: None,
        },
    };
    let update = operation == Operation::Update;
    let bundle = state::bundle(provider, version);
    record.pending = Some(operation.name().into());
    if update {
        state::verify_tree(&state.source, &record.files)?;
        state.save(&record)?;
        state.replace_source(&record.files, &bundle)?;
    } else {
        state.install_source(&record, &bundle)?;
        client.register(&state.source).await?;
        if !client.marketplace(&state.source).await? {
            return Err("provider did not list the newly registered marketplace".into());
        }
    }
    let returned_path = client.put(update).await?;
    let installation = client
        .installation(&state.source)
        .await?
        .ok_or("provider did not list the plugin after installation")?;
    check_installation(provider, &profile, &logical, &installation, version)?;
    let cache = provider.cache_version(&profile, version);
    if returned_path.is_some_and(|path| state::provider_path(&path) != cache.as_path()) {
        return Err("provider returned an unexpected installation path".into());
    }
    let files = state::plugin_files(&bundle);
    state::verify_cache_version(provider, &cache, &files)?;
    record.caches.insert(version.into(), files);
    prune_caches(provider, &profile, &mut record)?;
    record.files = bundle;
    record.version = version.into();
    record.pending = None;
    state.save(&record)?;
    Ok((
        if update { "updated" } else { "installed" },
        format!(
            "managed bundle {version}{}",
            if installation.enabled {
                ""
            } else {
                " (disabled)"
            }
        ),
    ))
}

fn check_installation(
    provider: Provider,
    profile: &Path,
    logical_profile: &Path,
    entry: &provider::Installation,
    version: &str,
) -> Result<()> {
    if entry.version == version {
        match &entry.path {
            None => return Ok(()),
            Some(path)
                if state::matches_install_path(
                    path,
                    &provider.cache_version(profile, version),
                    &provider.cache_version(logical_profile, version),
                )? =>
            {
                return Ok(());
            }
            Some(_) => {}
        }
    }
    Err(
        "provider reports an installation version or path that differs from the receipt or bundle"
            .into(),
    )
}

fn verify_cache(provider: Provider, profile: &Path, record: Option<&Receipt>) -> Result<()> {
    let root = provider.cache(profile);
    state::verify_cache_root(&root, |name| {
        let Some(record) = record else {
            return Err("foreign cache for the bundled marketplace".into());
        };
        let (version, expected) = record
            .caches
            .iter()
            .find(|(version, _)| provider.cache_version(profile, version).file_name() == Some(name))
            .ok_or("unrecorded cache version for the bundled plugin")?;
        state::verify_cache_version(
            provider,
            &provider.cache_version(profile, version),
            expected,
        )
    })?;
    if let Some(record) = record.filter(|record| record.active()) {
        let current = record
            .caches
            .get(&record.version)
            .ok_or("receipt has no current cache snapshot")?;
        // During update, Codex can remove the previous cache as part of re-add.
        if record.pending.is_none() {
            state::verify_cache_version(
                provider,
                &provider.cache_version(profile, &record.version),
                current,
            )?;
        }
    }
    Ok(())
}

fn prune_caches(provider: Provider, profile: &Path, record: &mut Receipt) -> Result<()> {
    verify_cache(provider, profile, Some(record))?;
    let mut missing = Vec::new();
    for version in record.caches.keys() {
        if state::metadata(&provider.cache_version(profile, version))?.is_none() {
            missing.push(version.clone());
        }
    }
    for version in missing {
        record.caches.remove(&version);
    }
    Ok(())
}
