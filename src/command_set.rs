//! A transport-neutral command model derived from the live MCP surface.
//!
//! MCP describes tools, prompts, resources, and resource templates in four
//! protocol-specific lists. Terminal front ends need a more uniform view:
//! named commands with arguments, plus values accepted by resource-oriented
//! commands. This module is the `mcp -> command AST` boundary; it deliberately
//! contains no clap, reedline, quoting, or dispatch behavior.

use std::collections::HashSet;

use tower_mcp::protocol::{
    PromptDefinition, ResourceDefinition, ResourceTemplateDefinition, ToolDefinition,
};

/// The discovered protocol surface and its immutable command projection.
///
/// Keeping the protocol lists private prevents the lazily cached command set
/// from becoming stale. A list-changed notification replaces this whole value.
#[derive(Default)]
pub(crate) struct Surface {
    tools: Vec<ToolDefinition>,
    prompts: Vec<PromptDefinition>,
    resources: Vec<ResourceDefinition>,
    templates: Vec<ResourceTemplateDefinition>,
    unavailable: Vec<&'static str>,
    command_set: std::sync::OnceLock<CommandSet>,
}

impl Surface {
    pub fn new(
        tools: Vec<ToolDefinition>,
        prompts: Vec<PromptDefinition>,
        resources: Vec<ResourceDefinition>,
        templates: Vec<ResourceTemplateDefinition>,
        unavailable: Vec<&'static str>,
    ) -> Self {
        Self {
            tools,
            prompts,
            resources,
            templates,
            unavailable,
            command_set: std::sync::OnceLock::new(),
        }
    }

    pub fn tools(&self) -> &[ToolDefinition] {
        &self.tools
    }

    pub fn prompts(&self) -> &[PromptDefinition] {
        &self.prompts
    }

    pub fn resources(&self) -> &[ResourceDefinition] {
        &self.resources
    }

    pub fn templates(&self) -> &[ResourceTemplateDefinition] {
        &self.templates
    }

    /// Whether a named part failed to load rather than coming back empty.
    pub fn is_unavailable(&self, what: &str) -> bool {
        self.unavailable.contains(&what)
    }

    pub fn commands(&self) -> &CommandSet {
        self.command_set
            .get_or_init(|| CommandSet::from_surface(self))
    }

    #[cfg(test)]
    pub fn tools_mut(&mut self) -> &mut Vec<ToolDefinition> {
        assert!(
            self.command_set.get().is_none(),
            "a normalized command set must not be mutated"
        );
        &mut self.tools
    }
}

/// The command-oriented projection of one discovered MCP surface.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CommandSet {
    pub tools: Vec<CommandSpec>,
    pub prompts: Vec<CommandSpec>,
    pub resources: Vec<ResourceSpec>,
    pub templates: Vec<ResourceSpec>,
}

/// One server-provided operation that accepts named arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommandSpec {
    pub name: String,
    pub description: Option<String>,
    pub arguments: Vec<ArgumentSpec>,
    /// Short capability/behavior labels used by command-list views.
    pub tags: Vec<String>,
}

impl CommandSpec {
    pub fn argument(&self, name: &str) -> Option<&ArgumentSpec> {
        self.arguments.iter().find(|argument| argument.name == name)
    }
}

/// A named command argument normalized from a tool schema or prompt argument.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ArgumentSpec {
    pub name: String,
    pub description: Option<String>,
    pub required: bool,
    pub value_type: Option<String>,
    pub choices: Vec<String>,
}

/// A concrete resource URI or a URI template accepted by resource commands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ResourceSpec {
    pub uri: String,
    pub name: String,
}

impl CommandSet {
    /// Normalize the current protocol surface once for any terminal view.
    fn from_surface(surface: &Surface) -> Self {
        let tools = surface
            .tools()
            .iter()
            .map(|tool| {
                let required: HashSet<&str> = tool
                    .input_schema
                    .get("required")
                    .and_then(serde_json::Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(serde_json::Value::as_str)
                    .collect();
                let arguments = tool
                    .input_schema
                    .get("properties")
                    .and_then(serde_json::Value::as_object)
                    .into_iter()
                    .flat_map(|properties| properties.iter())
                    .map(|(name, schema)| {
                        let resolved = resolve_ref(&tool.input_schema, schema);
                        ArgumentSpec {
                            name: name.clone(),
                            // A description beside `$ref` is more specific than
                            // the shared definition it points to.
                            description: schema
                                .get("description")
                                .or_else(|| resolved.get("description"))
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_string),
                            required: required.contains(name.as_str()),
                            value_type: resolved
                                .get("type")
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_string),
                            choices: enum_choices(resolved),
                        }
                    })
                    .collect();
                CommandSpec {
                    name: tool.name.clone(),
                    description: tool.description.clone(),
                    arguments,
                    tags: tool_tags(tool).into_iter().map(str::to_string).collect(),
                }
            })
            .collect();
        let prompts = surface
            .prompts()
            .iter()
            .map(|prompt| CommandSpec {
                name: prompt.name.clone(),
                description: prompt.description.clone(),
                arguments: prompt
                    .arguments
                    .iter()
                    .map(|argument| ArgumentSpec {
                        name: argument.name.clone(),
                        description: argument.description.clone(),
                        required: argument.required,
                        value_type: Some("string".to_string()),
                        choices: Vec::new(),
                    })
                    .collect(),
                tags: Vec::new(),
            })
            .collect();
        let resources = surface
            .resources()
            .iter()
            .map(|resource| ResourceSpec {
                uri: resource.uri.clone(),
                name: resource.name.clone(),
            })
            .collect();
        let templates = surface
            .templates()
            .iter()
            .map(|template| ResourceSpec {
                uri: template.uri_template.clone(),
                name: template.name.clone(),
            })
            .collect();
        Self {
            tools,
            prompts,
            resources,
            templates,
        }
    }

    pub fn tool(&self, name: &str) -> Option<&CommandSpec> {
        self.tools.iter().find(|command| command.name == name)
    }

    pub fn prompt(&self, name: &str) -> Option<&CommandSpec> {
        self.prompts.iter().find(|command| command.name == name)
    }
}

fn enum_choices(schema: &serde_json::Value) -> Vec<String> {
    schema
        .get("enum")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| value.to_string())
        })
        .collect()
}

/// Short safety and execution labels normalized from MCP tool metadata.
pub(crate) fn tool_tags(tool: &ToolDefinition) -> Vec<&'static str> {
    let mut tags = Vec::new();
    if let Some(annotations) = &tool.annotations {
        if annotations.read_only_hint {
            tags.push("read-only");
        }
        // A destructive read-only tool is a contradiction; trust read-only.
        if annotations.destructive_hint && !annotations.read_only_hint {
            tags.push("destructive");
        }
        if annotations.idempotent_hint {
            tags.push("idempotent");
        }
        if annotations.open_world_hint {
            tags.push("open-world");
        }
    }
    if let Some(execution) = &tool.execution {
        let value = serde_json::to_value(execution).unwrap_or_default();
        match value.get("taskSupport").and_then(serde_json::Value::as_str) {
            Some("required") => tags.push("task-only"),
            Some("optional") => tags.push("task-capable"),
            _ => {}
        }
    }
    tags
}

/// Follow a same-document JSON Schema `$ref` to the definition it names.
///
/// Remote refs are intentionally not fetched while building a command set;
/// an unresolvable ref degrades to the property schema supplied by the server.
pub(crate) fn resolve_ref<'a>(
    root: &'a serde_json::Value,
    schema: &'a serde_json::Value,
) -> &'a serde_json::Value {
    let Some(reference) = schema.get("$ref").and_then(serde_json::Value::as_str) else {
        return schema;
    };
    let Some(path) = reference.strip_prefix("#/") else {
        return schema;
    };
    let mut current = root;
    for segment in path.split('/') {
        let segment = segment.replace("~1", "/").replace("~0", "~");
        match current.get(&segment) {
            Some(next) => current = next,
            None => return schema,
        }
    }
    current
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_mcp::protocol::{PromptArgument, PromptDefinition, ToolDefinition};

    #[test]
    fn normalizes_tool_and_prompt_arguments_for_both_terminal_views() {
        let surface = Surface {
            tools: vec![ToolDefinition {
                name: "convert".to_string(),
                title: None,
                description: Some("Convert a value".to_string()),
                input_schema: serde_json::json!({
                    "type": "object",
                    "required": ["scale"],
                    "properties": {
                        "scale": {
                            "description": "Output scale",
                            "$ref": "#/$defs/Scale"
                        }
                    },
                    "$defs": {
                        "Scale": {"type": "string", "enum": ["c", "f"]}
                    }
                }),
                output_schema: None,
                icons: None,
                annotations: None,
                execution: None,
                meta: None,
            }],
            prompts: vec![PromptDefinition {
                name: "greet".to_string(),
                title: None,
                description: None,
                arguments: vec![PromptArgument {
                    name: "subject".to_string(),
                    description: Some("What to greet".to_string()),
                    required: true,
                }],
                icons: None,
                meta: None,
            }],
            ..Surface::default()
        };

        let commands = surface.commands();
        let scale = commands.tool("convert").unwrap().argument("scale").unwrap();
        assert!(scale.required);
        assert_eq!(scale.value_type.as_deref(), Some("string"));
        assert_eq!(scale.description.as_deref(), Some("Output scale"));
        assert_eq!(scale.choices, ["c", "f"]);

        let subject = commands
            .prompt("greet")
            .unwrap()
            .argument("subject")
            .unwrap();
        assert!(subject.required);
        assert_eq!(subject.value_type.as_deref(), Some("string"));
    }

    #[test]
    fn one_surface_reuses_one_normalized_command_set() {
        let surface = Surface::default();
        assert!(std::ptr::eq(surface.commands(), surface.commands()));
    }

    #[test]
    #[should_panic(expected = "a normalized command set must not be mutated")]
    fn test_mutation_cannot_stale_an_initialized_command_set() {
        let mut surface = Surface::default();
        let _ = surface.commands();
        let _ = surface.tools_mut();
    }
}
