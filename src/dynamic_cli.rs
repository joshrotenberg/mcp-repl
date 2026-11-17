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

/// A generated CLI invocation translated into the REPL's dispatch tokens.
/// Reusing that boundary keeps output, schema contracts, reconnects,
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

    pub(crate) fn parsed(&self) -> crate::command::ParsedCommand {
        crate::command::ParsedCommand {
            words: self.words.clone(),
            background: false,
        }
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

    if let Some(first) = argv
        .first()
        .filter(|first| !matches!(first.as_str(), "tool" | "builtin"))
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
            surface_invocation(Some("tool"), spec, matches)
        }
        "builtin" => {
            let (name, matches) = matches
                .subcommand()
                .expect("the builtin namespace requires a built-in");
            if ONE_SHOT_BUILTINS.contains(&name) {
                builtin_invocation(Some("builtin"), name, matches, &prompt_specs)
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
            surface_invocation(None, spec, matches)
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
        .after_help("Use --exec for other REPL built-ins, multi-command or stateful workflows.");
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
            words.extend(surface_arguments(spec, prompt_matches)?);
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
            .allow_negative_numbers(true)
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
) -> Result<Invocation, clap::Error> {
    let words = namespace
        .into_iter()
        .map(str::to_string)
        .chain(std::iter::once(spec.name.clone()))
        .chain(surface_arguments(spec, matches)?);
    Ok(Invocation::new(words))
}

fn surface_arguments(
    spec: &SurfaceCommandSpec,
    matches: &ArgMatches,
) -> Result<Vec<String>, clap::Error> {
    if spec.arguments.iter().any(|argument| {
        (argument.property.is_empty() || argument.property.contains('='))
            && matches.get_one::<String>(&argument.id).is_some()
    }) {
        return Err(clap::Error::raw(
            ErrorKind::InvalidValue,
            "this property name cannot use key=value dispatch; use call <raw-name> <JSON> instead",
        ));
    }
    Ok(spec
        .arguments
        .iter()
        .filter_map(|argument| {
            matches
                .get_one::<String>(&argument.id)
                .map(|value| format!("{}={value}", argument.property))
        })
        .collect())
}

fn spec_named<'a>(specs: &'a [SurfaceCommandSpec], command: &str) -> &'a SurfaceCommandSpec {
    specs
        .iter()
        .find(|spec| spec.command == command)
        .expect("every generated subcommand came from a surface spec")
}

fn command_specs(commands: &[CommandSpec], fallback: &str) -> Vec<SurfaceCommandSpec> {
    let names = stable_cli_names(
        commands.iter().map(|command| command.name.as_str()),
        fallback,
        &[],
    );
    commands
        .iter()
        .enumerate()
        .map(|(index, command)| SurfaceCommandSpec {
            command: names[index].clone(),
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
    let names = stable_cli_names(
        arguments.iter().map(|argument| argument.name.as_str()),
        "arg",
        &["help", "version"],
    );
    arguments
        .iter()
        .enumerate()
        .map(|(argument_index, argument)| ArgumentSpec {
            id: format!("surface-{owner_index}-arg-{argument_index}"),
            property: argument.name.clone(),
            flag: names[argument_index].clone(),
            required: argument.required,
            help: argument_help(argument),
            value_name: clean_text(argument.value_type.as_deref().unwrap_or("value"))
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

/// Allocate ordinary names first, then unusual names in raw-name order.
/// Reserving every ordinary name before suffix allocation keeps it intact,
/// including names such as `odd-name-2`, regardless of discovery order.
fn stable_cli_names<'a>(
    names: impl Iterator<Item = &'a str>,
    fallback: &str,
    reserved: &[&str],
) -> Vec<String> {
    let names: Vec<_> = names.collect();
    let mut order: Vec<_> = (0..names.len()).collect();
    order.sort_by_key(|&index| {
        (
            reserved.contains(&names[index]) || cli_base(names[index], fallback) != names[index],
            names[index],
        )
    });
    let mut used: HashSet<String> = reserved.iter().map(|name| name.to_string()).collect();
    let mut result = vec![String::new(); names.len()];
    for index in order {
        result[index] = unique_cli_name(names[index], fallback, &mut used);
    }
    result
}

fn cli_base(raw: &str, fallback: &str) -> String {
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
    base
}

fn unique_cli_name(raw: &str, fallback: &str, used: &mut HashSet<String>) -> String {
    let base = cli_base(raw, fallback);
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
            let parsed = invocation.parsed();
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
            read.parsed().words,
            ["read", "note://status", "--out", "result.txt", "--force"]
        );

        let prompt = parse(
            &[],
            &prompts,
            &["prompt".into(), "greet".into(), "--subject=world".into()],
        )
        .unwrap();
        assert_eq!(prompt.parsed().words, ["prompt", "greet", "subject=world"]);
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
    fn namespace_words_remain_reachable_when_the_server_uses_them() {
        let tools = [
            tool("tool", json!({"type":"object"})),
            tool("builtin", json!({"type":"object"})),
        ];
        for name in ["tool", "builtin"] {
            let invocation = parse(&tools, &[], &["tool", name].map(String::from)).unwrap();
            assert_eq!(invocation.parsed().words, ["tool", name]);
        }
        assert!(parse(&tools, &[], &["builtin", "tools"].map(String::from)).is_ok());
        assert_eq!(
            parse(&tools, &[], &["tool", "--help"].map(String::from))
                .unwrap_err()
                .kind(),
            ErrorKind::DisplayHelp
        );
    }

    #[test]
    fn stateful_builtins_require_exec() {
        assert!(parse(&[], &[], &["builtin".into(), "wait".into()]).is_err());
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
        assert_eq!(first.parsed().words, ["tool", "odd-name"]);
        assert_eq!(second.parsed().words, ["tool", "odd/name"]);
        let reserved = parse(&tools, &[], &["tool".into(), "tool".into()]).unwrap();
        assert_eq!(reserved.parsed().words, ["tool", "tool!"]);
    }
    #[test]
    fn names_and_flags_are_independent_of_discovery_order() {
        let names = [
            "odd/name",
            "odd-name",
            "odd-name-2",
            "odd?name",
            "",
            "help",
            "help-2",
            "version",
        ];
        let expected = stable_cli_names(names.iter().copied(), "arg", &["help", "version"]);
        for shift in 0..names.len() {
            let mut permuted = names.to_vec();
            permuted.rotate_left(shift);
            permuted.reverse();
            let actual = stable_cli_names(permuted.iter().copied(), "arg", &["help", "version"]);
            for (raw, flag) in permuted.iter().zip(actual) {
                assert_eq!(
                    flag,
                    expected[names.iter().position(|name| name == raw).unwrap()]
                );
            }
        }
        assert_eq!(expected[1], "odd-name");
        assert_eq!(expected[2], "odd-name-2");
        assert_eq!(expected[6], "help-2");
    }

    #[test]
    fn schema_values_reuse_common_coercion_and_keep_omissions() {
        let schema = json!({
            "type": "object",
            "$defs": {"Text": {"type": "string"}},
            "properties": {
                "count": {"type": "integer"},
                "enabled": {"type": "boolean"},
                "ratio": {"type": "number"},
                "nested": {"type": "object"},
                "items": {"type": "array"},
                "text": {"$ref": "#/$defs/Text"},
                "choice": {"type": "string", "enum": ["yes", "no"]},
                "nullable": {"type": ["string", "null"]},
                "omitted": {"type": "string", "default": "server default"}
            }
        });
        let tools = [tool("values", schema.clone())];
        let invocation = parse(
            &tools,
            &[],
            &[
                "values",
                "--count",
                "-3",
                "--ratio",
                "-0.5",
                "--enabled=false",
                "--nested={\"items\":[1,null]}",
                "--items=[true,2]",
                "--text=123",
                "--choice=yes",
                "--nullable=null",
            ]
            .map(String::from),
        )
        .unwrap();
        let parsed = invocation.parsed();
        let tokens: Vec<_> = parsed.words[1..].iter().map(String::as_str).collect();
        assert_eq!(
            crate::tool_args::parse_kv_args(&schema, &tokens).unwrap(),
            json!({
                "count": -3, "ratio": -0.5, "enabled": false, "nested": {"items": [1,null]},
                "items": [true,2], "text": "123", "choice": "yes", "nullable": null
            })
        );
        assert!(parse(&tools, &[], &["values", "--enabled"].map(String::from)).is_err());
        assert!(
            parse(
                &tools,
                &[],
                &["values", "--count=1", "--count=2"].map(String::from)
            )
            .is_err()
        );
    }

    #[test]
    fn reserved_flags_and_control_text_cannot_break_help() {
        let mut definition = tool(
            "help",
            json!({"type":"object", "properties": {
                "help": {"type":"string"}, "version": {"type":"string"}, "help-2": {"type":"string"}
            }}),
        );
        definition.description = Some("unsafe\u{1b}[31m description".into());
        let tools = [definition];
        let help = parse(&tools, &[], &["tool", "help", "--help"].map(String::from)).unwrap_err();
        assert_eq!(help.kind(), ErrorKind::DisplayHelp);
        assert!(!help.to_string().contains('\u{1b}'));
        let invocation = parse(
            &tools,
            &[],
            &["tool", "help", "--help-3=text", "--version-2=x"].map(String::from),
        )
        .unwrap();
        assert!(invocation.parsed().words.contains(&"help=text".to_string()));
    }

    #[test]
    fn malformed_property_names_require_raw_json_instead_of_wrong_arguments() {
        let tools = [tool(
            "odd",
            json!({"type":"object", "properties": {"a=b": {"type":"string"}}}),
        )];
        assert!(
            parse(&tools, &[], &["odd", "--a-b=text"].map(String::from))
                .unwrap_err()
                .to_string()
                .contains("call")
        );
        let invocation = parse(
            &tools,
            &[],
            &["call", "odd", "{\"a=b\":\"text\"}"].map(String::from),
        )
        .unwrap();
        assert_eq!(
            invocation.parsed().words,
            ["call", "odd", "{\"a=b\":\"text\"}"]
        );
    }
}
