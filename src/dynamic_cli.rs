//! Runtime CLI generation from a connected server's surface.
//!
//! The startup parser cannot know these commands: discovering them requires
//! an MCP connection. Explicit connection modes can leave their positional
//! tail untouched, however, so this module gives that tail a second clap pass
//! after the surface is fetched. The command vocabulary mirrors the REPL;
//! only its argument form changes from `key=value` to ordinary CLI flags.

use std::collections::HashSet;

use clap::{Arg, ArgAction, ArgMatches, Command, error::ErrorKind};

use crate::command_set::{CommandSet, CommandSpec};

const LIST_BUILTINS: &[&str] = &["tools", "prompts", "resources", "templates"];
const ONE_SHOT_BUILTINS: &[&str] = &[
    "tools",
    "prompts",
    "resources",
    "templates",
    "find",
    "describe",
    "snapshot",
    "validate",
    "read",
    "prompt",
    "call",
];

/// A generated CLI invocation translated back into the REPL's command
/// language. Reusing that boundary keeps output, schema contracts, reconnects,
/// and exit statuses identical to `--exec` and interactive calls.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Invocation {
    words: Vec<String>,
}

impl Invocation {
    fn new(words: impl IntoIterator<Item = String>) -> Self {
        Self {
            words: words.into_iter().collect(),
        }
    }

    pub(crate) fn repl_line(&self) -> String {
        self.words
            .iter()
            // JSON string quoting is accepted by the REPL tokenizer and
            // safely preserves whitespace, quotes, backslashes, and `&`.
            .map(|word| serde_json::to_string(word).expect("strings always serialize"))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[derive(Clone, Debug)]
struct ArgumentSpec {
    id: String,
    property: String,
    flag: String,
    required: bool,
    help: Option<String>,
    value_name: String,
}

#[derive(Clone, Debug)]
struct SurfaceCommandSpec {
    command: String,
    name: String,
    about: Option<String>,
    arguments: Vec<ArgumentSpec>,
}

/// Parse one command using a clap tree assembled from the connected surface.
pub(crate) fn parse(commands: &CommandSet, argv: &[String]) -> Result<Invocation, clap::Error> {
    let tool_specs = command_specs(&commands.tools, "tool");
    let prompt_specs = command_specs(&commands.prompts, "prompt");
    let mut root = Command::new("mcp-repl")
        .bin_name("mcp-repl <connection options>")
        .about("Run one command against the connected MCP server")
        .subcommand_help_heading("COMMANDS")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .disable_help_subcommand(true);

    root = root.subcommand(tool_namespace(&tool_specs));
    root = root.subcommand(builtin_namespace(&prompt_specs));
    for name in ONE_SHOT_BUILTINS {
        root = root.subcommand(builtin_command(name, &prompt_specs));
    }
    // Bare tool names are the same convenience they are in the REPL. A tool
    // colliding with a built-in is deliberately absent here and handled by
    // the ambiguity check below; both explicit namespaces remain available.
    for spec in tool_specs.iter().filter(|spec| {
        !crate::is_builtin(&spec.name)
            && !ONE_SHOT_BUILTINS.contains(&spec.command.as_str())
            && !matches!(spec.command.as_str(), "tool" | "builtin")
    }) {
        root = root.subcommand(surface_command(spec));
    }

    if let Some(first) = argv.first()
        && let Some(spec) = tool_specs
            .iter()
            .find(|spec| spec.command == *first && crate::is_builtin(&spec.name))
    {
        return Err(root.error(
            ErrorKind::InvalidSubcommand,
            format!(
                "ambiguous command `{}`: both a server tool and a built-in use that name; \
                 use `tool {} ...` for the server tool or `builtin {} ...` for the built-in",
                clean_text(&spec.name),
                clean_text(&spec.command),
                clean_text(&spec.name),
            ),
        ));
    }

    let matches = root.try_get_matches_from(
        std::iter::once("mcp-repl".to_string()).chain(argv.iter().cloned()),
    )?;
    let (command, matches) = matches
        .subcommand()
        .expect("clap enforces a generated subcommand");
    match command {
        "tool" => {
            let (name, matches) = matches
                .subcommand()
                .expect("the tool namespace requires a server tool");
            let spec = spec_named(&tool_specs, name);
            Ok(surface_invocation(Some("tool"), spec, matches))
        }
        "builtin" => {
            let (name, matches) = matches
                .subcommand()
                .expect("the builtin namespace requires a built-in");
            if ONE_SHOT_BUILTINS.contains(&name) {
                builtin_invocation(Some("builtin"), name, matches, &prompt_specs)
            } else if crate::is_builtin(name) {
                let words = ["builtin".to_string(), name.to_string()].into_iter().chain(
                    matches
                        .get_many::<String>("")
                        .into_iter()
                        .flatten()
                        .cloned(),
                );
                Ok(Invocation::new(words))
            } else {
                Err(clap::Error::raw(
                    ErrorKind::InvalidSubcommand,
                    format!("no REPL built-in named `{}`", clean_text(name)),
                ))
            }
        }
        name if ONE_SHOT_BUILTINS.contains(&name) => {
            builtin_invocation(None, name, matches, &prompt_specs)
        }
        name => {
            let spec = spec_named(&tool_specs, name);
            Ok(surface_invocation(None, spec, matches))
        }
    }
}

fn tool_namespace(specs: &[SurfaceCommandSpec]) -> Command {
    let mut command = Command::new("tool")
        .about("Run a server tool explicitly")
        .subcommand_help_heading("SERVER TOOLS")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .disable_help_subcommand(true);
    for spec in specs {
        command = command.subcommand(surface_command(spec));
    }
    command
}

fn builtin_namespace(prompt_specs: &[SurfaceCommandSpec]) -> Command {
    let mut command = Command::new("builtin")
        .about("Run a REPL built-in explicitly")
        .subcommand_help_heading("ONE-SHOT BUILT-INS")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .disable_help_subcommand(true)
        .allow_external_subcommands(true)
        .external_subcommand_value_parser(clap::value_parser!(String))
        .after_help(
            "Other REPL built-ins are accepted with their REPL argument syntax; use --exec for \
             multi-command or stateful workflows.",
        );
    for name in ONE_SHOT_BUILTINS {
        command = command.subcommand(builtin_command(name, prompt_specs));
    }
    command
}

fn builtin_command(name: &str, prompt_specs: &[SurfaceCommandSpec]) -> Command {
    match name {
        name if LIST_BUILTINS.contains(&name) => Command::new(name.to_string())
            .about(format!("List the server's {name}"))
            .arg(
                Arg::new("full")
                    .long("full")
                    .help("Print the complete listing")
                    .action(ArgAction::SetTrue),
            ),
        "read" => Command::new("read")
            .about("Read a resource")
            .arg(
                Arg::new("uri")
                    .value_name("URI")
                    .help("Concrete resource URI, including an expanded template URI")
                    .required(true),
            )
            .arg(
                Arg::new("out")
                    .long("out")
                    .value_name("PATH")
                    .help("Write the returned content to a file"),
            )
            .arg(
                Arg::new("force")
                    .long("force")
                    .help("Overwrite an existing output file")
                    .action(ArgAction::SetTrue),
            ),
        "prompt" => {
            let mut command = Command::new("prompt")
                .about("Get a server prompt")
                .subcommand_help_heading("SERVER PROMPTS")
                .subcommand_required(true)
                .arg_required_else_help(true)
                .disable_help_subcommand(true);
            for spec in prompt_specs {
                command = command.subcommand(surface_command(spec));
            }
            command
        }
        "call" => Command::new("call")
            .about("Call a tool with a raw JSON argument object")
            .arg(Arg::new("tool").value_name("TOOL").required(true))
            .arg(
                Arg::new("json")
                    .value_name("JSON")
                    .required(true)
                    .allow_hyphen_values(true),
            ),
        "describe" => Command::new("describe")
            .about("Show schemas and metadata for a surface item")
            .arg(Arg::new("name").value_name("NAME").required(true)),
        "snapshot" => Command::new("snapshot")
            .about("Export a tool or prompt schema contract")
            .arg(Arg::new("name").value_name("NAME").required(true))
            .arg(Arg::new("path").value_name("PATH")),
        "validate" => Command::new("validate")
            .about("Compare the live surface with a schema snapshot")
            .arg(Arg::new("path").value_name("PATH").required(true))
            .arg(Arg::new("mode").value_name("MODE").value_parser([
                "strict",
                "compatible",
                "ignore",
            ])),
        "find" => Command::new("find")
            .about("Search the connected server's surface")
            .arg(
                Arg::new("regex")
                    .short('E')
                    .help("Treat the query as a regular expression")
                    .action(ArgAction::SetTrue),
            )
            .arg(Arg::new("max").short('m').long("max").value_name("COUNT"))
            .arg(
                Arg::new("case-sensitive")
                    .long("case-sensitive")
                    .action(ArgAction::SetTrue),
            )
            .arg(kind_flag("tools"))
            .arg(kind_flag("prompts"))
            .arg(kind_flag("resources"))
            .arg(kind_flag("templates"))
            .arg(kind_flag("builtins"))
            .arg(
                Arg::new("query")
                    .value_name("QUERY")
                    .required(true)
                    .num_args(1..)
                    .action(ArgAction::Append),
            ),
        _ => unreachable!("ONE_SHOT_BUILTINS contains only defined commands"),
    }
}

fn kind_flag(name: &'static str) -> Arg {
    Arg::new(name).long(name).action(ArgAction::SetTrue)
}

fn builtin_invocation(
    namespace: Option<&str>,
    name: &str,
    matches: &ArgMatches,
    prompt_specs: &[SurfaceCommandSpec],
) -> Result<Invocation, clap::Error> {
    let mut words = namespace
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
    words.push(name.to_string());
    match name {
        name if LIST_BUILTINS.contains(&name) => {
            if matches.get_flag("full") {
                words.push("--full".to_string());
            }
        }
        "read" => {
            words.push(required_string(matches, "uri"));
            if let Some(path) = matches.get_one::<String>("out") {
                words.extend(["--out".to_string(), path.clone()]);
            }
            if matches.get_flag("force") {
                words.push("--force".to_string());
            }
        }
        "prompt" => {
            let (prompt, prompt_matches) = matches
                .subcommand()
                .expect("the prompt command requires a server prompt");
            let spec = spec_named(prompt_specs, prompt);
            words.push(spec.name.clone());
            words.extend(surface_arguments(spec, prompt_matches));
        }
        "call" => {
            words.push(required_string(matches, "tool"));
            words.push(required_string(matches, "json"));
        }
        "describe" => words.push(required_string(matches, "name")),
        "snapshot" => {
            words.push(required_string(matches, "name"));
            words.extend(matches.get_one::<String>("path").cloned());
        }
        "validate" => {
            words.push(required_string(matches, "path"));
            words.extend(matches.get_one::<String>("mode").cloned());
        }
        "find" => {
            if matches.get_flag("regex") {
                words.push("-E".to_string());
            }
            if let Some(max) = matches.get_one::<String>("max") {
                words.extend(["--max".to_string(), max.clone()]);
            }
            for flag in [
                "case-sensitive",
                "tools",
                "prompts",
                "resources",
                "templates",
                "builtins",
            ] {
                if matches.get_flag(flag) {
                    words.push(format!("--{flag}"));
                }
            }
            words.extend(
                matches
                    .get_many::<String>("query")
                    .expect("the find query is required")
                    .cloned(),
            );
        }
        _ => unreachable!("ONE_SHOT_BUILTINS contains only defined commands"),
    }
    Ok(Invocation::new(words))
}

fn required_string(matches: &ArgMatches, id: &str) -> String {
    matches
        .get_one::<String>(id)
        .expect("clap enforces required values")
        .clone()
}

fn surface_command(spec: &SurfaceCommandSpec) -> Command {
    let mut command = Command::new(spec.command.clone());
    if let Some(about) = &spec.about {
        command = command.about(about.clone());
    }
    if spec.command != spec.name {
        command = command.long_about(format!(
            "{}\n\nMCP name: {}",
            spec.about.as_deref().unwrap_or("Server-provided MCP item"),
            clean_text(&spec.name)
        ));
    }
    for argument in &spec.arguments {
        let mut arg = Arg::new(argument.id.clone())
            .long(argument.flag.clone())
            .value_name(argument.value_name.clone())
            .required(argument.required)
            .num_args(1)
            .action(ArgAction::Set);
        if let Some(help) = &argument.help {
            arg = arg.help(help.clone());
        }
        command = command.arg(arg);
    }
    command
}

fn surface_invocation(
    namespace: Option<&str>,
    spec: &SurfaceCommandSpec,
    matches: &ArgMatches,
) -> Invocation {
    let words = namespace
        .into_iter()
        .map(str::to_string)
        .chain(std::iter::once(spec.name.clone()))
        .chain(surface_arguments(spec, matches));
    Invocation::new(words)
}

fn surface_arguments(spec: &SurfaceCommandSpec, matches: &ArgMatches) -> Vec<String> {
    spec.arguments
        .iter()
        .filter_map(|argument| {
            matches
                .get_one::<String>(&argument.id)
                .map(|value| format!("{}={value}", argument.property))
        })
        .collect()
}

fn spec_named<'a>(specs: &'a [SurfaceCommandSpec], command: &str) -> &'a SurfaceCommandSpec {
    specs
        .iter()
        .find(|spec| spec.command == command)
        .expect("every generated subcommand came from a surface spec")
}

fn command_specs(commands: &[CommandSpec], fallback: &str) -> Vec<SurfaceCommandSpec> {
    let mut used_commands = HashSet::new();
    commands
        .iter()
        .enumerate()
        .map(|(index, command)| SurfaceCommandSpec {
            command: unique_cli_name(&command.name, fallback, &mut used_commands),
            name: command.name.clone(),
            about: command.description.as_deref().map(clean_text),
            arguments: argument_specs(index, &command.arguments),
        })
        .collect()
}

fn argument_specs(
    owner_index: usize,
    arguments: &[crate::command_set::ArgumentSpec],
) -> Vec<ArgumentSpec> {
    let mut used_flags = HashSet::from(["help".to_string(), "version".to_string()]);
    arguments
        .iter()
        .enumerate()
        .map(|(argument_index, argument)| ArgumentSpec {
            id: format!("surface-{owner_index}-arg-{argument_index}"),
            property: argument.name.clone(),
            flag: unique_cli_name(&argument.name, "arg", &mut used_flags),
            required: argument.required,
            help: argument_help(argument),
            value_name: argument
                .value_type
                .as_deref()
                .unwrap_or("value")
                .to_ascii_uppercase(),
        })
        .collect()
}

fn argument_help(argument: &crate::command_set::ArgumentSpec) -> Option<String> {
    let description = argument.description.as_deref().map(clean_text);
    let choices = (!argument.choices.is_empty()).then(|| {
        argument
            .choices
            .iter()
            .map(|choice| clean_text(choice))
            .collect::<Vec<_>>()
            .join(", ")
    });
    match (description, choices) {
        (Some(description), Some(choices)) => Some(format!("{description} [values: {choices}]")),
        (Some(description), None) => Some(description),
        (None, Some(choices)) => Some(format!("Allowed values: {choices}")),
        (None, None) => None,
    }
}

/// Preserve normal MCP names exactly and make unusual names safe for clap.
/// Collisions receive stable numeric suffixes rather than hiding an item.
fn unique_cli_name(raw: &str, fallback: &str, used: &mut HashSet<String>) -> String {
    let mut base = raw
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-') {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>();
    while base.starts_with('-') {
        base.remove(0);
    }
    while base.ends_with('-') {
        base.pop();
    }
    if base.is_empty() {
        base = fallback.to_string();
    }
    let mut candidate = base.clone();
    let mut suffix = 2;
    while !used.insert(candidate.clone()) {
        candidate = format!("{base}-{suffix}");
        suffix += 1;
    }
    candidate
}

fn clean_text(value: &str) -> String {
    crate::untrusted::sanitize(value).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tower_mcp::protocol::{PromptArgument, PromptDefinition, ToolDefinition};

    fn parse(
        tools: &[ToolDefinition],
        prompts: &[PromptDefinition],
        argv: &[String],
    ) -> Result<Invocation, clap::Error> {
        let surface = crate::Surface::new(
            tools.to_vec(),
            prompts.to_vec(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );
        super::parse(surface.commands(), argv)
    }

    fn tool(name: &str, schema: serde_json::Value) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            title: None,
            description: Some("A generated tool".to_string()),
            input_schema: schema,
            output_schema: None,
            icons: None,
            annotations: None,
            execution: None,
            meta: None,
        }
    }

    fn prompt(name: &str) -> PromptDefinition {
        PromptDefinition {
            name: name.to_string(),
            title: None,
            description: Some("A generated prompt".to_string()),
            arguments: vec![PromptArgument {
                name: "subject".to_string(),
                description: Some("What to greet".to_string()),
                required: true,
            }],
            icons: None,
            meta: None,
        }
    }

    fn echo() -> ToolDefinition {
        tool(
            "echo",
            json!({
                "type": "object",
                "required": ["message"],
                "properties": {
                    "message": {"type": "string", "description": "Text to echo"},
                    "repeat": {"type": "integer"}
                }
            }),
        )
    }

    #[test]
    fn direct_and_explicit_tool_forms_translate_to_the_repl_vocabulary() {
        let tools = [echo()];
        for argv in [
            vec![
                "echo".into(),
                "--message=hello world".into(),
                "--repeat=2".into(),
            ],
            vec![
                "tool".into(),
                "echo".into(),
                "--message=hello world".into(),
                "--repeat=2".into(),
            ],
        ] {
            let invocation = parse(&tools, &[], &argv).unwrap();
            let parsed = crate::command::parse(&invocation.repl_line()).unwrap();
            let expected_prefix = if argv[0] == "tool" {
                vec!["tool".to_string(), "echo".to_string()]
            } else {
                vec!["echo".to_string()]
            };
            assert!(parsed.words.starts_with(&expected_prefix));
            assert!(parsed.words.contains(&"message=hello world".to_string()));
            assert!(parsed.words.contains(&"repeat=2".to_string()));
        }
    }

    #[test]
    fn read_and_prompt_keep_the_repl_command_names() {
        let prompts = [prompt("greet")];
        let read = parse(
            &[],
            &prompts,
            &[
                "read".into(),
                "note://status".into(),
                "--out=result.txt".into(),
                "--force".into(),
            ],
        )
        .unwrap();
        assert_eq!(
            crate::command::parse(&read.repl_line()).unwrap().words,
            ["read", "note://status", "--out", "result.txt", "--force"]
        );

        let prompt = parse(
            &[],
            &prompts,
            &["prompt".into(), "greet".into(), "--subject=world".into()],
        )
        .unwrap();
        assert_eq!(
            crate::command::parse(&prompt.repl_line()).unwrap().words,
            ["prompt", "greet", "subject=world"]
        );
    }

    #[test]
    fn tool_builtin_collisions_require_the_existing_namespaces() {
        let tools = [tool("read", json!({"type": "object"}))];
        let error = parse(&tools, &[], &["read".into(), "note://status".into()]).unwrap_err();
        assert!(error.to_string().contains("tool read"));
        assert!(error.to_string().contains("builtin read"));
        assert!(parse(&tools, &[], &["tool".into(), "read".into()]).is_ok());
        assert!(
            parse(
                &tools,
                &[],
                &["builtin".into(), "read".into(), "note://status".into()]
            )
            .is_ok()
        );
    }

    #[test]
    fn explicit_builtin_keeps_stateful_repl_commands_reachable() {
        let invocation = parse(
            &[],
            &[],
            &[
                "builtin".into(),
                "wait".into(),
                "last".into(),
                "--timeout".into(),
                "1".into(),
            ],
        )
        .unwrap();
        assert_eq!(
            crate::command::parse(&invocation.repl_line())
                .unwrap()
                .words,
            ["builtin", "wait", "last", "--timeout", "1"]
        );
    }

    #[test]
    fn required_properties_and_unknown_tools_are_clap_errors() {
        let tools = [echo()];
        assert_eq!(
            parse(&tools, &[], &["echo".into()]).unwrap_err().kind(),
            ErrorKind::MissingRequiredArgument
        );
        assert!(parse(&tools, &[], &["nope".into()]).is_err());
    }

    #[test]
    fn unsafe_and_colliding_names_stay_addressable_in_the_tool_namespace() {
        let tools = [
            tool("odd/name", json!({"type": "object"})),
            tool("odd-name", json!({"type": "object"})),
            tool("tool!", json!({"type": "object"})),
        ];
        let first = parse(&tools, &[], &["tool".into(), "odd-name".into()]).unwrap();
        let second = parse(&tools, &[], &["tool".into(), "odd-name-2".into()]).unwrap();
        assert_eq!(
            crate::command::parse(&first.repl_line()).unwrap().words,
            ["tool", "odd/name"]
        );
        assert_eq!(
            crate::command::parse(&second.repl_line()).unwrap().words,
            ["tool", "odd-name"]
        );
        let reserved = parse(&tools, &[], &["tool".into(), "tool".into()]).unwrap();
        assert_eq!(
            crate::command::parse(&reserved.repl_line()).unwrap().words,
            ["tool", "tool!"]
        );
    }
}
