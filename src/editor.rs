//! The reedline editor: prompt, completer, live highlighter, and the
//! dedicated readline thread.
//!
//! The editor runs on its own std thread; lines cross into async via a
//! tokio mpsc channel and an ack channel paces the prompt so it only
//! reappears after the previous command finished printing. The completer
//! blocks on this thread, which lives outside the tokio runtime, so
//! `Handle::block_on` is safe here.

use std::borrow::Cow;
use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use nu_ansi_term::{Color, Style};
use reedline::{
    ColumnarMenu, Completer, DefaultHinter, Emacs, ExternalPrinter, FileBackedHistory, Highlighter,
    KeyCode, KeyModifiers, MenuBuilder, Prompt, PromptEditMode, PromptHistorySearch,
    PromptHistorySearchStatus, Reedline, ReedlineEvent, ReedlineMenu, Signal, Span, StyledText,
    Suggestion, ValidationResult, Validator, default_emacs_keybindings,
};
use tower_mcp::protocol::ToolDefinition;

use crate::session::Session;

use crate::alias::Aliases;
use crate::style;
use crate::{BUILTINS, Surface};

const MENU_NAME: &str = "completion_menu";

/// How long a server gets to answer `completion/complete` mid-word.
///
/// Short on purpose. This runs between keystrokes on the readline thread, so
/// a slow server would otherwise make Tab feel broken rather than merely
/// unhelpful. Overridable with `[repl] completion_timeout_ms`.
pub const DEFAULT_COMPLETION_TIMEOUT: Duration = Duration::from_secs(2);

static COMPLETION_TIMEOUT_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Set once at startup, from the config.
pub fn set_completion_timeout(timeout: Duration) {
    COMPLETION_TIMEOUT_MS.store(timeout.as_millis() as u64, Ordering::Relaxed);
}

fn completion_timeout() -> Duration {
    match COMPLETION_TIMEOUT_MS.load(Ordering::Relaxed) {
        0 => DEFAULT_COMPLETION_TIMEOUT,
        ms => Duration::from_millis(ms),
    }
}

/// Keeps reading while the line is unfinished.
///
/// Without this, pasting a pretty-printed JSON body submits it one line at
/// a time and the first line fails on a brace the rest of the paste was
/// about to supply. The tokenizer already knows the difference between
/// unfinished and wrong, so a mismatched delimiter still errors at once
/// rather than trapping the editor in a continuation.
struct ReplValidator;

impl Validator for ReplValidator {
    fn validate(&self, line: &str) -> ValidationResult {
        if crate::command::is_incomplete(line) {
            ValidationResult::Incomplete
        } else {
            ValidationResult::Complete
        }
    }
}

// ---------------------------------------------------------------------------
// Prompt
// ---------------------------------------------------------------------------

struct ReplPrompt {
    server_name: crate::elicit::ServerLabel,
}

impl Prompt for ReplPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Owned(self.server_name.read().unwrap().clone())
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_indicator(&self, _prompt_mode: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed("> ")
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("::: ")
    }

    fn render_prompt_history_search_indicator(
        &self,
        history_search: PromptHistorySearch,
    ) -> Cow<'_, str> {
        let status = match history_search.status {
            PromptHistorySearchStatus::Passing => "",
            PromptHistorySearchStatus::Failing => "failing ",
        };
        Cow::Owned(format!(
            "({}reverse-search: {}) ",
            status, history_search.term
        ))
    }
}

// ---------------------------------------------------------------------------
// Completer
// ---------------------------------------------------------------------------

pub struct ReplCompleter {
    surface: Arc<RwLock<Surface>>,
    /// The session rather than the client: a reconnect swaps the client out,
    /// and completions issued afterwards must go to the live one.
    session: Arc<Session>,
    aliases: Arc<RwLock<Aliases>>,
    runtime: tokio::runtime::Handle,
}

/// Every completion candidate is built here, including ones assembled from
/// server-supplied names, descriptions, and `completion/complete` values, so
/// this is where control sequences are neutralized before reedline paints
/// them into the menu.
/// Follow a local `$ref` to the definition it names.
///
/// Schema generators split named types out into `$defs` and leave a `$ref`
/// behind, so a Rust or Python server describing an enum argument sends
/// `{"$ref": "#/$defs/Scale"}` rather than the values inline. Without this,
/// completion works only for servers that happen to inline everything.
///
/// Only same-document refs are followed: this runs while the user is typing,
/// and fetching a remote schema is neither fast nor safe. A ref that does not
/// resolve yields the original schema, so an unusual shape degrades to no
/// completion rather than a wrong one.
pub(crate) fn resolve_ref<'a>(
    root: &'a serde_json::Value,
    schema: &'a serde_json::Value,
) -> &'a serde_json::Value {
    let Some(reference) = schema.get("$ref").and_then(|r| r.as_str()) else {
        return schema;
    };
    let Some(path) = reference.strip_prefix("#/") else {
        return schema;
    };
    let mut current = root;
    for segment in path.split('/') {
        // JSON Pointer escapes, in the order the spec requires.
        let segment = segment.replace("~1", "/").replace("~0", "~");
        match current.get(&segment) {
            Some(next) => current = next,
            None => return schema,
        }
    }
    current
}

fn suggestion(value: impl Into<String>, description: Option<String>, span: Span) -> Suggestion {
    Suggestion {
        value: style::sanitize(&value.into()).into_owned(),
        display_override: None,
        description: description.map(|d| style::sanitize(&d).into_owned()),
        style: None,
        extra: None,
        span,
        append_whitespace: false,
        match_indices: None,
    }
}

fn word_suggestion(
    value: impl Into<String>,
    description: Option<String>,
    span: Span,
) -> Suggestion {
    Suggestion {
        append_whitespace: true,
        ..suggestion(value, description, span)
    }
}

impl ReplCompleter {
    pub fn new(
        surface: Arc<RwLock<Surface>>,
        session: Arc<Session>,
        aliases: Arc<RwLock<Aliases>>,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        Self {
            surface,
            session,
            aliases,
            runtime,
        }
    }

    /// Server-powered completion for prompt argument values
    /// (`completion/complete` with a prompt reference). Safe to block here:
    /// the completer runs on the dedicated readline thread, outside the
    /// async runtime.
    fn complete_prompt_arg_via_server(
        &self,
        prompt: &str,
        arg: &str,
        partial: &str,
    ) -> Vec<String> {
        let Some(client) = self.session.try_client() else {
            return Vec::new();
        };
        let (prompt, arg, partial) = (prompt.to_string(), arg.to_string(), partial.to_string());
        self.runtime
            .block_on(async move {
                tokio::time::timeout(
                    completion_timeout(),
                    client.complete_prompt_arg(&prompt, &arg, &partial),
                )
                .await
            })
            .ok()
            .and_then(|r| r.ok())
            .map(|r| r.completion.values)
            .unwrap_or_default()
    }

    /// Server-powered completion for a resource template variable
    /// (`completion/complete` with a resource reference). Same blocking
    /// pattern as prompt arguments: 2s timeout, best-effort.
    fn complete_template_var_via_server(
        &self,
        uri_template: &str,
        var: &str,
        partial: &str,
    ) -> Vec<String> {
        let Some(client) = self.session.try_client() else {
            return Vec::new();
        };
        let (template, var, partial) = (
            uri_template.to_string(),
            var.to_string(),
            partial.to_string(),
        );
        self.runtime
            .block_on(async move {
                tokio::time::timeout(
                    completion_timeout(),
                    client.complete_resource_uri(&template, &var, &partial),
                )
                .await
            })
            .ok()
            .and_then(|r| r.ok())
            .map(|r| r.completion.values)
            .unwrap_or_default()
    }

    /// Completions for `read <partial>`: literal resource URIs, template
    /// URI templates, and server-completed template variables when the
    /// partial has reached a template's `{variable}`.
    fn complete_resource_word(&self, surface: &Surface, word: &str, span: Span) -> Vec<Suggestion> {
        let mut out = Vec::new();
        for r in &surface.resources {
            if r.uri.starts_with(word) {
                out.push(word_suggestion(&r.uri, Some(r.name.clone()), span));
            }
        }
        for t in &surface.templates {
            if t.uri_template.starts_with(word) {
                out.push(suggestion(&t.uri_template, Some(t.name.clone()), span));
            }
            // Template variable completion: `file:///{path}` with word
            // `file:///src/` asks the server to complete `path` = `src/`.
            let Some(open) = t.uri_template.find('{') else {
                continue;
            };
            let Some(close_rel) = t.uri_template[open..].find('}') else {
                continue;
            };
            let close = open + close_rel;
            let static_prefix = &t.uri_template[..open];
            if word.len() < static_prefix.len() || !word.starts_with(static_prefix) {
                continue;
            }
            let var = &t.uri_template[open + 1..close];
            let suffix = &t.uri_template[close + 1..];
            let partial_value = &word[static_prefix.len()..];
            for v in self.complete_template_var_via_server(&t.uri_template, var, partial_value) {
                let mut full = format!("{static_prefix}{v}");
                if !suffix.contains('{') {
                    full.push_str(suffix);
                }
                out.push(suggestion(full, Some(format!("{var} ({})", t.name)), span));
            }
        }
        out
    }

    /// Completions for a word in a tool's argument list: argument names from
    /// the tool's `inputSchema` properties, and enum values after `=`.
    fn complete_tool_arg_word(
        surface: &Surface,
        tool_name: &str,
        word: &str,
        span: Span,
    ) -> Vec<Suggestion> {
        let mut out = Vec::new();
        let Some(tool) = surface.tools.iter().find(|t| t.name == tool_name) else {
            return out;
        };
        let Some(props) = tool
            .input_schema
            .get("properties")
            .and_then(|p| p.as_object())
        else {
            return out;
        };
        if let Some((arg_name, partial)) = word.split_once('=') {
            // Enum values from the property schema, when declared.
            if let Some(values) = props
                .get(arg_name)
                .map(|schema| resolve_ref(&tool.input_schema, schema))
                .and_then(|s| s.get("enum").cloned())
                .as_ref()
                .and_then(|e| e.as_array())
            {
                for v in values {
                    if let Some(v) = v.as_str()
                        && v.starts_with(partial)
                    {
                        out.push(word_suggestion(format!("{arg_name}={v}"), None, span));
                    }
                }
            }
            return out;
        }
        let required: Vec<&str> = tool
            .input_schema
            .get("required")
            .and_then(|r| r.as_array())
            .map(|r| r.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        for (key, prop) in props {
            if !key.starts_with(word) {
                continue;
            }
            // The description stays on the property (a `$ref` sibling keeps
            // it), but the type lives in the definition it points at.
            let target = resolve_ref(&tool.input_schema, prop);
            let ty = target.get("type").and_then(|t| t.as_str()).unwrap_or("");
            let desc = prop
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("");
            let req = if required.contains(&key.as_str()) {
                "required "
            } else {
                ""
            };
            let full = format!("{req}{ty} {desc}");
            let full = full.trim();
            let desc = (!full.is_empty()).then(|| full.to_string());
            out.push(suggestion(format!("{key}="), desc, span));
        }
        out
    }

    fn tool_description(tool: &ToolDefinition) -> Option<String> {
        let tags = crate::tool_tags(tool);
        match (tool.description.as_deref(), tags.is_empty()) {
            (_, false) => Some(format!(
                "{}{}[{}]",
                tool.description.as_deref().unwrap_or(""),
                if tool.description.is_some() { "  " } else { "" },
                tags.join(" ")
            )),
            (description, true) => description.map(str::to_string),
        }
    }

    fn complete_tool_name_word(surface: &Surface, word: &str, span: Span) -> Vec<Suggestion> {
        surface
            .tools
            .iter()
            .filter(|tool| tool.name.starts_with(word))
            .map(|tool| word_suggestion(&tool.name, Self::tool_description(tool), span))
            .collect()
    }

    fn complete_builtin_name_word(word: &str, span: Span) -> Vec<Suggestion> {
        BUILTINS
            .iter()
            .filter(|builtin| builtin.name.starts_with(word))
            .map(|builtin| word_suggestion(builtin.name, Some(builtin.summary.to_string()), span))
            .collect()
    }

    fn complete_command_word(
        surface: &Surface,
        aliases: &Aliases,
        word: &str,
        span: Span,
    ) -> Vec<Suggestion> {
        let mut out = Vec::new();
        for builtin in BUILTINS.iter() {
            let (name, description) = (builtin.name, builtin.summary);
            if !name.starts_with(word) {
                continue;
            }
            let description = if crate::is_ambiguous_command(surface, name) {
                format!("ambiguous; use `tool {name}` or `builtin {name}`")
            } else {
                (*description).to_string()
            };
            out.push(word_suggestion(name, Some(description), span));
        }
        for entry in aliases.entries() {
            if entry.name.starts_with(word) {
                out.push(word_suggestion(
                    entry.name,
                    Some(format!("alias for `{}`", entry.expansion)),
                    span,
                ));
            }
        }
        // A colliding bare spelling is represented once above as ambiguous,
        // rather than twice as though either duplicate would be runnable.
        for tool in &surface.tools {
            if tool.name.starts_with(word) && !crate::is_builtin(&tool.name) {
                out.push(word_suggestion(
                    &tool.name,
                    Self::tool_description(tool),
                    span,
                ));
            }
        }
        out
    }

    /// Completions for `describe <name>`: every named thing on the surface.
    fn complete_describe_word(surface: &Surface, word: &str, span: Span) -> Vec<Suggestion> {
        let mut out = Vec::new();
        for t in &surface.tools {
            if t.name.starts_with(word) {
                out.push(word_suggestion(&t.name, Some("tool".to_string()), span));
            }
        }
        for p in &surface.prompts {
            if p.name.starts_with(word) {
                out.push(word_suggestion(&p.name, Some("prompt".to_string()), span));
            }
        }
        for r in &surface.resources {
            if r.uri.starts_with(word) {
                out.push(word_suggestion(&r.uri, Some("resource".to_string()), span));
            }
        }
        for t in &surface.templates {
            if t.uri_template.starts_with(word) {
                out.push(word_suggestion(
                    &t.uri_template,
                    Some("template".to_string()),
                    span,
                ));
            }
        }
        for builtin in BUILTINS.iter() {
            let (name, description) = (builtin.name, builtin.summary);
            let already_named = surface.tools.iter().any(|tool| tool.name == *name)
                || surface.prompts.iter().any(|prompt| prompt.name == *name)
                || surface
                    .resources
                    .iter()
                    .any(|resource| resource.name == *name || resource.uri == *name)
                || surface
                    .templates
                    .iter()
                    .any(|template| template.name == *name || template.uri_template == *name);
            if name.starts_with(word) && !already_named {
                out.push(word_suggestion(
                    name,
                    Some(format!("built-in: {description}")),
                    span,
                ));
            }
        }
        out
    }
}

impl Completer for ReplCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        let head = &line[..pos];
        let (word_start, word) = match head.rfind(char::is_whitespace) {
            Some(i) => (i + 1, &head[i + 1..]),
            None => (0, head),
        };
        let span = Span::new(word_start, pos);
        let surface = self.surface.read().unwrap();
        let mut out: Vec<Suggestion> = Vec::new();

        let first = head.split_whitespace().next().unwrap_or("");
        let completing_first = word_start == 0;

        if completing_first {
            // First word: built-ins, every alias, and every tool name.
            out.extend(Self::complete_command_word(
                &surface,
                &self.aliases.read().unwrap(),
                word,
                span,
            ));
            // The listings take one flag; offer it once the user types a dash.
            if word.starts_with('-') && "--full".starts_with(word) {
                out.push(word_suggestion(
                    "--full",
                    Some("print every row, ignoring the window height".to_string()),
                    span,
                ));
            }
            return out;
        }

        match first {
            "tool" => {
                let words = head.split_whitespace().count();
                let naming_tool = words == 1 || (words == 2 && !head.ends_with(' '));
                if naming_tool {
                    out.extend(Self::complete_tool_name_word(&surface, word, span));
                } else if let Some(tool_name) = head.split_whitespace().nth(1) {
                    out.extend(Self::complete_tool_arg_word(
                        &surface, tool_name, word, span,
                    ));
                }
            }
            "builtin" => {
                let words = head.split_whitespace().count();
                let naming_builtin = words == 1 || (words == 2 && !head.ends_with(' '));
                if naming_builtin {
                    out.extend(Self::complete_builtin_name_word(word, span));
                }
            }
            // `find` has its own flags; offer them once a dash is typed.
            "find" if word.starts_with('-') => {
                for (flag, description) in [
                    ("-E", "treat the keyword as a regular expression"),
                    ("-m", "cap the number of results"),
                    ("--case-sensitive", "do not fold case"),
                    ("--tools", "search tools only"),
                    ("--prompts", "search prompts only"),
                    ("--resources", "search resources only"),
                    ("--templates", "search resource templates only"),
                    ("--builtins", "search the REPL's own commands only"),
                ] {
                    if flag.starts_with(word) {
                        out.push(word_suggestion(flag, Some(description.to_string()), span));
                    }
                }
            }
            // `read` writes the payload to a file with --out; offer both
            // flags once a dash is typed.
            "read" if word.starts_with('-') => {
                for (flag, description) in [
                    (
                        "--out",
                        "write the content to a file instead of printing it",
                    ),
                    ("--force", "overwrite the file if it exists"),
                ] {
                    if flag.starts_with(word) {
                        out.push(word_suggestion(flag, Some(description.to_string()), span));
                    }
                }
            }
            "read" | "subscribe" => {
                out.extend(self.complete_resource_word(&surface, word, span));
            }
            // Only what is actually subscribed can be unsubscribed.
            "unsubscribe" => {
                for uri in crate::subscribe::list() {
                    if uri.starts_with(word) {
                        out.push(word_suggestion(uri, None, span));
                    }
                }
            }
            // The levels are a fixed scale the server does not choose, so
            // offering them saves a trip to `help`.
            "loglevel" => {
                for level in crate::LOG_LEVELS {
                    if level.starts_with(word) {
                        out.push(word_suggestion(*level, None, span));
                    }
                }
            }
            "wire" => {
                for state in ["on", "off"] {
                    if state.starts_with(word) {
                        out.push(word_suggestion(state, None, span));
                    }
                }
            }
            // `task <id> respond` is the only subcommand, and it is easy to
            // miss: offer it once the task is named. Before that, and for the
            // other task commands, offer `last`.
            "task" | "wait" | "cancel" if head.split_whitespace().count() >= 2 => {
                let naming_task = head.split_whitespace().count() == 2
                    && !head.ends_with(' ')
                    && !word.is_empty();
                if naming_task || head.split_whitespace().count() == 1 {
                    if "last".starts_with(word) {
                        out.push(word_suggestion(
                            "last",
                            Some("the most recently started task".to_string()),
                            span,
                        ));
                    }
                } else if first == "task" && "respond".starts_with(word) {
                    out.push(word_suggestion(
                        "respond",
                        Some("answer what the task is waiting for".to_string()),
                        span,
                    ));
                }
            }
            "describe" | "snapshot" => {
                out.extend(Self::complete_describe_word(&surface, word, span));
            }
            "unalias" => {
                for entry in self.aliases.read().unwrap().entries() {
                    if entry.name.starts_with(word) {
                        out.push(word_suggestion(
                            entry.name,
                            Some(format!("{} ({})", entry.expansion, entry.scope.label())),
                            span,
                        ));
                    }
                }
            }
            "prompt" => {
                let words = head.split_whitespace().count();
                let second_word = words == 2 && !head.ends_with(' ');
                let naming_prompt = second_word || words == 1;
                if naming_prompt {
                    for p in &surface.prompts {
                        if p.name.starts_with(word) {
                            out.push(word_suggestion(&p.name, p.description.clone(), span));
                        }
                    }
                } else if let Some(prompt_name) = head.split_whitespace().nth(1) {
                    if let Some((arg_name, partial)) = word.split_once('=') {
                        // Argument value: ask the server (completion/complete).
                        for v in self.complete_prompt_arg_via_server(prompt_name, arg_name, partial)
                        {
                            out.push(word_suggestion(format!("{arg_name}={v}"), None, span));
                        }
                    } else if let Some(p) = surface.prompts.iter().find(|p| p.name == prompt_name) {
                        // Argument name: from the prompt definition.
                        for a in &p.arguments {
                            if a.name.starts_with(word) {
                                let desc = match (&a.description, a.required) {
                                    (Some(d), true) => Some(format!("(required) {d}")),
                                    (Some(d), false) => Some(d.clone()),
                                    (None, true) => Some("(required)".to_string()),
                                    (None, false) => None,
                                };
                                out.push(suggestion(format!("{}=", a.name), desc, span));
                            }
                        }
                    }
                }
            }
            "call" => {
                for t in &surface.tools {
                    if t.name.starts_with(word) {
                        out.push(word_suggestion(&t.name, t.description.clone(), span));
                    }
                }
            }
            "bench" => {
                // `bench <tool> [k=v...] [--n N] [--concurrency C]`: the tool
                // name first, then that tool's arguments, so it completes the
                // same way calling the tool directly does.
                let words = head.split_whitespace().count();
                let naming_tool = words == 1 || (words == 2 && !head.ends_with(' '));
                if word.starts_with('-') {
                    for flag in ["--n", "--concurrency"] {
                        if flag.starts_with(word) {
                            out.push(word_suggestion(flag, None, span));
                        }
                    }
                } else if naming_tool {
                    for t in &surface.tools {
                        if t.name.starts_with(word) {
                            out.push(word_suggestion(&t.name, t.description.clone(), span));
                        }
                    }
                } else if let Some(tool_name) = head.split_whitespace().nth(1) {
                    out.extend(Self::complete_tool_arg_word(
                        &surface, tool_name, word, span,
                    ));
                }
            }
            tool_name => {
                out.extend(Self::complete_tool_arg_word(
                    &surface, tool_name, word, span,
                ));
            }
        }

        out
    }
}

// ---------------------------------------------------------------------------
// Highlighter
// ---------------------------------------------------------------------------

/// Live input highlighting: the command word is styled by validity
/// (built-in, known tool, or unknown), `key=` argument names are cyan,
/// and JSON-literal values get the same palette as output rendering.
pub struct ReplHighlighter {
    surface: Arc<RwLock<Surface>>,
    aliases: Arc<RwLock<Aliases>>,
}

impl ReplHighlighter {
    pub fn new(surface: Arc<RwLock<Surface>>, aliases: Arc<RwLock<Aliases>>) -> Self {
        Self { surface, aliases }
    }

    fn command_style(&self, word: &str) -> Style {
        let surface = self.surface.read().unwrap();
        if crate::is_ambiguous_command(&surface, word) {
            return Style::new().fg(Color::Yellow).bold();
        }
        if crate::is_builtin(word) {
            return Style::new().fg(Color::Cyan).bold();
        }
        drop(surface);
        // An alias resolves to a command, so it reads as one: same style as a
        // built-in, since that is what it behaves like at the prompt.
        let aliases = self.aliases.read().unwrap();
        if aliases.lookup(word).is_some() {
            return Style::new().fg(Color::Cyan).bold();
        }
        let surface = self.surface.read().unwrap();
        if surface.tools.iter().any(|t| t.name == word) {
            return Style::new().fg(Color::Green).bold();
        }
        // Prefix of something completable: neutral while typing.
        let is_prefix = BUILTINS.any_starts_with(word)
            || aliases.entries().iter().any(|e| e.name.starts_with(word))
            || surface.tools.iter().any(|t| t.name.starts_with(word));
        if is_prefix {
            Style::new()
        } else {
            Style::new().fg(Color::Red)
        }
    }

    fn qualified_name_style(&self, qualifier: &str, word: &str) -> Style {
        match qualifier {
            "tool" => {
                let surface = self.surface.read().unwrap();
                if crate::is_tool(&surface, word) {
                    Style::new().fg(Color::Green).bold()
                } else if surface.tools.iter().any(|tool| tool.name.starts_with(word)) {
                    Style::new()
                } else {
                    Style::new().fg(Color::Red)
                }
            }
            "builtin" if crate::is_builtin(word) => Style::new().fg(Color::Cyan).bold(),
            "builtin" if BUILTINS.any_starts_with(word) => Style::new(),
            _ => Style::new().fg(Color::Red),
        }
    }
}

fn value_style(raw: &str) -> Style {
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(serde_json::Value::Number(_)) => Style::new().fg(Color::Yellow),
        Ok(serde_json::Value::Bool(_)) | Ok(serde_json::Value::Null) => {
            Style::new().fg(Color::Purple)
        }
        Ok(serde_json::Value::String(_)) => Style::new().fg(Color::Green),
        Ok(_) => Style::new().fg(Color::Green).dimmed(),
        Err(_) => Style::new(),
    }
}

impl Highlighter for ReplHighlighter {
    fn highlight(&self, line: &str, _cursor: usize) -> StyledText {
        let mut styled = StyledText::new();
        if !style::colors_enabled() {
            styled.push((Style::new(), line.to_string()));
            return styled;
        }

        let mut seen_command = false;
        let mut qualifier = None;
        let mut seen_qualified_name = false;
        let mut rest = line;
        while !rest.is_empty() {
            let token_start = match rest.find(|c: char| !c.is_whitespace()) {
                Some(i) => i,
                None => {
                    styled.push((Style::new(), rest.to_string()));
                    break;
                }
            };
            if token_start > 0 {
                styled.push((Style::new(), rest[..token_start].to_string()));
            }
            let token_end = rest[token_start..]
                .find(char::is_whitespace)
                .map(|i| token_start + i)
                .unwrap_or(rest.len());
            let token = &rest[token_start..token_end];

            if !seen_command {
                styled.push((self.command_style(token), token.to_string()));
                seen_command = true;
                if matches!(token, "tool" | "builtin") {
                    qualifier = Some(token);
                }
            } else if let Some(namespace) = qualifier
                && !seen_qualified_name
            {
                styled.push((
                    self.qualified_name_style(namespace, token),
                    token.to_string(),
                ));
                seen_qualified_name = true;
            } else if token == "&" {
                styled.push((Style::new().fg(Color::Purple).bold(), token.to_string()));
            } else if let Some((key, value)) = token.split_once('=') {
                styled.push((Style::new().fg(Color::Cyan), format!("{key}=")));
                styled.push((value_style(value), value.to_string()));
            } else {
                styled.push((value_style(token), token.to_string()));
            }

            rest = &rest[token_end..];
        }
        if line.is_empty() {
            styled.push((Style::new(), String::new()));
        }
        styled
    }
}

// ---------------------------------------------------------------------------
// Readline thread
// ---------------------------------------------------------------------------

/// Spawn the readline thread. Lines are sent over `line_tx`; after each
/// line, the thread blocks on `ack_rx` until the command has finished
/// printing. `at_prompt` is true while the editor owns the terminal
/// (used to refuse elicitation prompts that would fight over stdin).
///
/// When stdin is not a tty (piped input), falls back to a plain
/// line-reader so non-interactive use keeps working.
#[allow(clippy::too_many_arguments)]
pub fn spawn_readline_thread(
    server_name: crate::elicit::ServerLabel,
    surface: Arc<RwLock<Surface>>,
    session: Arc<Session>,
    aliases: Arc<RwLock<Aliases>>,
    runtime: tokio::runtime::Handle,
    line_tx: tokio::sync::mpsc::Sender<String>,
    ack_rx: std::sync::mpsc::Receiver<()>,
    at_prompt: Arc<AtomicBool>,
    external_printer: ExternalPrinter<String>,
    persist_history: bool,
    history_capacity: usize,
) {
    std::thread::spawn(move || {
        if !std::io::stdin().is_terminal() {
            run_piped(&line_tx, &ack_rx);
            return;
        }
        run_interactive(
            server_name,
            surface,
            session,
            aliases,
            runtime,
            &line_tx,
            &ack_rx,
            at_prompt,
            external_printer,
            persist_history,
            history_capacity,
        );
    });
}

/// Entries kept when the config does not say otherwise.
pub const DEFAULT_HISTORY_CAPACITY: usize = 1000;

/// The command-history file, shared across sessions.
///
/// On Unix this is `$XDG_STATE_HOME/mcp-repl/history`, falling back to
/// `~/.local/state/mcp-repl/history`. On Windows it is below
/// `%LOCALAPPDATA%\mcp-repl`. `None` when no platform state location is
/// available, which keeps history in memory for the session.
pub fn history_path() -> Option<std::path::PathBuf> {
    history_path_with(&crate::directories::Directories::current())
}

fn history_path_with(directories: &crate::directories::Directories) -> Option<std::path::PathBuf> {
    directories.history_file()
}

/// The most recent history entries, newest last, for the `history` command.
///
/// Read from the file rather than from the editor: the editor lives on the
/// readline thread and the command runs on the async side, and a history
/// listing is not worth a channel round trip.
pub fn recent_history(limit: usize) -> Vec<String> {
    let path = history_path();
    recent_history_at(path.as_deref(), limit)
}

fn recent_history_at(path: Option<&std::path::Path>, limit: usize) -> Vec<String> {
    let Some(path) = path else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    lines
        .iter()
        .rev()
        .take(limit)
        .rev()
        .map(|line| (*line).to_string())
        .collect()
}

/// Where history used to live, for one-time migration.
fn legacy_history_path() -> Option<std::path::PathBuf> {
    crate::directories::Directories::current().legacy_history_file()
}

/// Move a pre-XDG history file to the new location, once.
///
/// Only when the new path does not exist: a user who has already built up
/// history at the new location keeps it, and the old file is left alone
/// rather than silently deleted.
fn migrate_legacy_history(destination: &std::path::Path) {
    if destination.exists() {
        return;
    }
    let Some(legacy) = legacy_history_path() else {
        return;
    };
    if !legacy.is_file() {
        return;
    }
    if let Err(e) = crate::secure_file::create_parent_dir(destination) {
        eprintln!("warning: could not create the history directory: {e}");
        return;
    }
    match std::fs::rename(&legacy, destination) {
        Ok(()) => eprintln!(
            "note: moved command history to {} (it now follows the XDG state layout)",
            destination.display()
        ),
        // Crossing a filesystem boundary, most likely. Copying leaves the
        // original in place, which is the safe direction for a file the
        // user may care about.
        Err(_) => match std::fs::copy(&legacy, destination) {
            Ok(_) => eprintln!(
                "note: copied command history to {}; the old {} can be deleted",
                destination.display(),
                legacy.display()
            ),
            Err(e) => eprintln!("warning: could not migrate command history: {e}"),
        },
    }
}

fn run_piped(line_tx: &tokio::sync::mpsc::Sender<String>, ack_rx: &std::sync::mpsc::Receiver<()>) {
    let stdin = std::io::stdin();
    run_piped_with(
        |buf| {
            // Scope the stdin lock to one read: holding it while waiting on
            // the ack channel would deadlock the elicitation handler, which
            // reads stdin during a foreground tool call.
            let mut lock = stdin.lock();
            std::io::BufRead::read_line(&mut lock, buf)
        },
        line_tx,
        ack_rx,
    );
}

fn run_piped_with(
    mut read_line: impl FnMut(&mut String) -> std::io::Result<usize>,
    line_tx: &tokio::sync::mpsc::Sender<String>,
    ack_rx: &std::sync::mpsc::Receiver<()>,
) {
    let mut pending = String::new();
    loop {
        let mut line = String::new();
        match read_line(&mut line) {
            Ok(0) | Err(_) => {
                if !pending.is_empty() && !dispatch_piped(&mut pending, line_tx, ack_rx) {
                    break;
                }
                let _ = line_tx.blocking_send("quit".to_string());
                break;
            }
            Ok(_) => {
                pending.push_str(&line);
                if crate::command::is_incomplete(&pending) {
                    continue;
                }
                if !dispatch_piped(&mut pending, line_tx, ack_rx) {
                    break;
                }
            }
        }
    }
}

fn dispatch_piped(
    pending: &mut String,
    line_tx: &tokio::sync::mpsc::Sender<String>,
    ack_rx: &std::sync::mpsc::Receiver<()>,
) -> bool {
    let mut command = std::mem::take(pending);
    if command.ends_with('\n') {
        command.pop();
        if command.ends_with('\r') {
            command.pop();
        }
    }
    line_tx.blocking_send(command).is_ok() && ack_rx.recv().is_ok()
}

#[allow(clippy::too_many_arguments)]
fn run_interactive(
    server_name: crate::elicit::ServerLabel,
    surface: Arc<RwLock<Surface>>,
    session: Arc<Session>,
    aliases: Arc<RwLock<Aliases>>,
    runtime: tokio::runtime::Handle,
    line_tx: &tokio::sync::mpsc::Sender<String>,
    ack_rx: &std::sync::mpsc::Receiver<()>,
    at_prompt: Arc<AtomicBool>,
    external_printer: ExternalPrinter<String>,
    persist_history: bool,
    history_capacity: usize,
) {
    let completer = ReplCompleter::new(surface.clone(), session, aliases.clone(), runtime);
    let highlighter = ReplHighlighter::new(surface, aliases);

    let menu = ColumnarMenu::default().with_name(MENU_NAME);
    let mut keybindings = default_emacs_keybindings();
    keybindings.add_binding(
        KeyModifiers::NONE,
        KeyCode::Tab,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::Menu(MENU_NAME.to_string()),
            ReedlineEvent::MenuNext,
        ]),
    );

    let mut editor = Reedline::create()
        .with_validator(Box::new(ReplValidator))
        .with_completer(Box::new(completer))
        .with_menu(ReedlineMenu::EngineCompleter(Box::new(menu)))
        .with_edit_mode(Box::new(Emacs::new(keybindings)))
        .with_highlighter(Box::new(highlighter))
        .with_hinter(Box::new(
            DefaultHinter::default().with_style(Style::new().fg(Color::DarkGray)),
        ))
        .with_external_printer(external_printer)
        .with_ansi_colors(style::colors_enabled());

    // Persist history across sessions (up to 1000 entries) so up-arrow recalls
    // commands from previous runs. Best-effort: an unwritable platform state
    // directory just keeps history in-memory for this session.
    if persist_history && let Some(path) = history_path() {
        migrate_legacy_history(&path);
        // Every typed line lands here, including tool arguments carrying
        // tokens, so the file is owner-only before reedline opens it:
        // reedline creates it with whatever the umask allows.
        if let Err(e) = crate::secure_file::ensure_owner_only(&path) {
            eprintln!("warning: could not secure the history file: {e}");
        }
        match FileBackedHistory::with_file(history_capacity, path) {
            Ok(history) => editor = editor.with_history(Box::new(history)),
            Err(e) => eprintln!("warning: command history disabled: {e}"),
        }
    }

    let prompt = ReplPrompt { server_name };
    loop {
        at_prompt.store(true, Ordering::SeqCst);
        let sig = editor.read_line(&prompt);
        at_prompt.store(false, Ordering::SeqCst);
        match sig {
            Ok(Signal::Success(line)) => {
                // Flush history now: a typed `quit` exits the process from the
                // main loop, so this thread's editor is never dropped and
                // FileBackedHistory would otherwise not persist on exit.
                let _ = editor.sync_history();
                if line_tx.blocking_send(line).is_err() {
                    break;
                }
                // Wait until the command finished printing before showing
                // the next prompt.
                if ack_rx.recv().is_err() {
                    break;
                }
            }
            Ok(Signal::CtrlC) => continue,
            _ => {
                let _ = line_tx.blocking_send("quit".to_string());
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directories::{Directories, Platform};
    use std::collections::BTreeMap;
    use std::ffi::OsString;

    fn surface_with_colliding_wait() -> Surface {
        Surface {
            tools: vec![
                serde_json::from_value(serde_json::json!({
                    "name": "wait",
                    "description": "Wait on the server",
                    "inputSchema": {
                        "type": "object",
                        "properties": {"id": {"type": "integer"}},
                    },
                }))
                .expect("tool definition"),
            ],
            ..Default::default()
        }
    }

    /// A schema shaped the way a generator emits one: named types hoisted
    /// into `$defs`, referenced from each property.
    fn schema_with_defs() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "to": {"$ref": "#/$defs/Scale", "description": "Target scale"},
                "value": {"type": "number"},
            },
            "$defs": {
                "Scale": {"type": "string", "enum": ["celsius", "fahrenheit"]},
            },
        })
    }

    fn surface_for_editor_paths() -> Surface {
        Surface {
            tools: vec![
                serde_json::from_value(serde_json::json!({
                    "name": "convert",
                    "description": "Convert safely\u{001b}]52;c;hostile\u{0007}",
                    "inputSchema": schema_with_defs(),
                }))
                .expect("tool definition"),
            ],
            prompts: vec![
                serde_json::from_value(serde_json::json!({
                    "name": "greet",
                    "description": "Generate a greeting",
                    "arguments": [{"name": "name", "required": true}],
                }))
                .expect("prompt definition"),
            ],
            resources: vec![
                serde_json::from_value(serde_json::json!({
                    "uri": "note://status",
                    "name": "Status",
                }))
                .expect("resource definition"),
            ],
            templates: vec![
                serde_json::from_value(serde_json::json!({
                    "uriTemplate": "note://{name}",
                    "name": "Notes",
                }))
                .expect("resource template definition"),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn validator_separates_unfinished_input_from_complete_or_impossible_input() {
        let validator = ReplValidator;
        assert!(matches!(
            validator.validate("call echo {"),
            ValidationResult::Incomplete
        ));
        assert!(matches!(
            validator.validate(r#"call echo {"message":"hi"}"#),
            ValidationResult::Complete
        ));
        assert!(matches!(
            validator.validate("call echo {1]"),
            ValidationResult::Complete
        ));
    }

    #[test]
    fn piped_input_accumulates_multiline_commands_and_keeps_command_boundaries() {
        let mut input =
            std::io::Cursor::new("call echo {\n  \"message\": \"hello from a pipe\"\n}\ntools\n");
        let (line_tx, mut line_rx) = tokio::sync::mpsc::channel(4);
        let (ack_tx, ack_rx) = std::sync::mpsc::channel();
        ack_tx.send(()).unwrap();
        ack_tx.send(()).unwrap();

        run_piped_with(
            |buf| std::io::BufRead::read_line(&mut input, buf),
            &line_tx,
            &ack_rx,
        );

        assert_eq!(
            line_rx.blocking_recv().as_deref(),
            Some("call echo {\n  \"message\": \"hello from a pipe\"\n}")
        );
        assert_eq!(line_rx.blocking_recv().as_deref(), Some("tools"));
        assert_eq!(line_rx.blocking_recv().as_deref(), Some("quit"));
    }

    #[test]
    fn piped_input_submits_an_unfinished_final_command_at_eof() {
        let mut input = std::io::Cursor::new("call echo {\n");
        let (line_tx, mut line_rx) = tokio::sync::mpsc::channel(2);
        let (ack_tx, ack_rx) = std::sync::mpsc::channel();
        ack_tx.send(()).unwrap();

        run_piped_with(
            |buf| std::io::BufRead::read_line(&mut input, buf),
            &line_tx,
            &ack_rx,
        );

        assert_eq!(line_rx.blocking_recv().as_deref(), Some("call echo {"));
        assert_eq!(line_rx.blocking_recv().as_deref(), Some("quit"));
    }

    #[test]
    fn tool_argument_completion_is_schema_driven_and_sanitized() {
        let surface = surface_for_editor_paths();
        let names = ReplCompleter::complete_tool_arg_word(&surface, "convert", "", Span::new(8, 8));
        let to = names
            .iter()
            .find(|suggestion| suggestion.value == "to=")
            .expect("the referenced enum property completes");
        assert_eq!(to.description.as_deref(), Some("string Target scale"));
        assert!(!to.append_whitespace, "a value still belongs after `=`");

        let values =
            ReplCompleter::complete_tool_arg_word(&surface, "convert", "to=f", Span::new(8, 12));
        assert_eq!(
            values
                .iter()
                .map(|suggestion| suggestion.value.as_str())
                .collect::<Vec<_>>(),
            ["to=fahrenheit"]
        );
        assert!(values[0].append_whitespace);

        let tools = ReplCompleter::complete_tool_name_word(&surface, "con", Span::new(0, 3));
        let description = tools[0].description.as_deref().unwrap_or_default();
        assert!(!description.contains('\u{1b}'), "{description:?}");
        assert!(description.contains('\u{fffd}'), "{description:?}");
    }

    #[test]
    fn describe_completion_covers_each_surface_kind_without_builtin_duplicates() {
        let surface = surface_for_editor_paths();
        for (partial, expected, kind) in [
            ("con", "convert", "tool"),
            ("gre", "greet", "prompt"),
            ("note://s", "note://status", "resource"),
            ("note://{", "note://{name}", "template"),
            ("hel", "help", "built-in"),
        ] {
            let suggestions =
                ReplCompleter::complete_describe_word(&surface, partial, Span::new(0, 0));
            let found = suggestions
                .iter()
                .find(|suggestion| suggestion.value == expected)
                .unwrap_or_else(|| panic!("missing {expected:?} for {partial:?}"));
            assert!(
                found
                    .description
                    .as_deref()
                    .unwrap_or_default()
                    .contains(kind),
                "{found:?}"
            );
        }
    }

    #[test]
    fn highlighter_styles_aliases_prefixes_values_and_unknown_names_without_a_terminal() {
        let aliases = Aliases::new(
            BTreeMap::from([("cv".to_string(), "convert".to_string())]),
            BTreeMap::new(),
            None,
            None,
        );
        let highlighter = ReplHighlighter::new(
            Arc::new(RwLock::new(surface_for_editor_paths())),
            Arc::new(RwLock::new(aliases)),
        );
        assert_eq!(
            highlighter.command_style("cv"),
            Style::new().fg(Color::Cyan).bold()
        );
        assert_eq!(highlighter.command_style("con"), Style::new());
        assert_eq!(
            highlighter.command_style("unknown"),
            Style::new().fg(Color::Red)
        );
        assert_eq!(value_style("42"), Style::new().fg(Color::Yellow));
        assert_eq!(value_style("true"), Style::new().fg(Color::Purple));
        assert_eq!(value_style("plain"), Style::new());
    }

    #[test]
    fn history_follows_the_xdg_state_layout() {
        let dir = tempfile::tempdir().unwrap();
        let directories = Directories::from_lookup(Platform::Unix, |name| {
            (name == "XDG_STATE_HOME").then(|| dir.path().as_os_str().to_owned())
        });
        let path = history_path_with(&directories).expect("a path");
        assert_eq!(path, dir.path().join("mcp-repl").join("history"));
        // Not the old dotfile, and not under the config directory: history
        // is state.
        assert!(!path.to_string_lossy().contains(".mcp-repl_history"));
    }

    #[test]
    fn windows_directories_drive_the_history_path() {
        let directories = Directories::from_lookup(Platform::Windows, |name| {
            (name == "LOCALAPPDATA").then(|| OsString::from(r"C:\Users\Ada\AppData\Local"))
        });
        assert_eq!(
            history_path_with(&directories),
            Some(
                std::path::PathBuf::from(r"C:\Users\Ada\AppData\Local")
                    .join("mcp-repl")
                    .join("history")
            )
        );
    }

    #[test]
    fn recent_history_returns_the_newest_entries_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("mcp-repl").join("history");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(
            &file,
            "first
second

third
fourth
",
        )
        .unwrap();

        let all = recent_history_at(Some(&file), 10);
        // Blank lines are not commands.
        assert_eq!(all, vec!["first", "second", "third", "fourth"]);

        // A limit takes the newest, still oldest-first on screen.
        let tail = recent_history_at(Some(&file), 2);
        assert_eq!(tail, vec!["third", "fourth"]);
    }

    #[test]
    fn no_history_file_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(recent_history_at(Some(&dir.path().join("missing")), 10).is_empty());
    }

    #[test]
    fn migration_does_not_overwrite_an_existing_history() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("history");
        std::fs::write(
            &destination,
            "already here
",
        )
        .unwrap();
        // Whatever the legacy path holds, a populated destination wins:
        // losing accumulated history to a migration would be worse than
        // leaving an old file behind.
        migrate_legacy_history(&destination);
        assert_eq!(
            std::fs::read_to_string(&destination).unwrap(),
            "already here\n"
        );
    }

    #[test]
    fn a_local_ref_resolves_to_its_definition() {
        let root = schema_with_defs();
        let property = &root["properties"]["to"];
        let resolved = resolve_ref(&root, property);
        assert_eq!(resolved["type"], "string");
        assert_eq!(resolved["enum"][0], "celsius");
    }

    #[test]
    fn a_schema_without_a_ref_is_returned_unchanged() {
        let root = schema_with_defs();
        let property = &root["properties"]["value"];
        assert_eq!(resolve_ref(&root, property)["type"], "number");
    }

    #[test]
    fn an_unresolvable_ref_degrades_to_the_property_itself() {
        // Better to offer no completion than to follow a ref somewhere
        // unexpected, and a remote ref is never fetched while typing.
        let root = schema_with_defs();
        for reference in [
            serde_json::json!({"$ref": "#/$defs/Missing"}),
            serde_json::json!({"$ref": "https://example.com/schema.json"}),
        ] {
            assert_eq!(resolve_ref(&root, &reference), &reference);
        }
    }

    #[test]
    fn json_pointer_escapes_are_decoded() {
        let root = serde_json::json!({
            "$defs": {"a/b~c": {"type": "integer"}},
        });
        let reference = serde_json::json!({"$ref": "#/$defs/a~1b~0c"});
        assert_eq!(resolve_ref(&root, &reference)["type"], "integer");
    }

    #[test]
    fn a_colliding_command_completion_is_truthful_and_not_duplicated() {
        let surface = surface_with_colliding_wait();
        let aliases = Aliases::default();
        let suggestions =
            ReplCompleter::complete_command_word(&surface, &aliases, "wai", Span::new(0, 3));
        assert_eq!(
            suggestions
                .iter()
                .filter(|suggestion| suggestion.value == "wait")
                .count(),
            1
        );
        let wait = suggestions
            .iter()
            .find(|suggestion| suggestion.value == "wait")
            .expect("wait completion");
        let description = wait.description.as_deref().unwrap_or_default();
        assert!(description.contains("tool wait"), "{description}");
        assert!(description.contains("builtin wait"), "{description}");
    }

    #[test]
    fn both_explicit_namespaces_complete_their_own_names() {
        let surface = surface_with_colliding_wait();
        let tools = ReplCompleter::complete_tool_name_word(&surface, "wai", Span::new(5, 8));
        assert!(tools.iter().any(|suggestion| suggestion.value == "wait"));

        let builtins = ReplCompleter::complete_builtin_name_word("wai", Span::new(8, 11));
        assert!(builtins.iter().any(|suggestion| suggestion.value == "wait"));
    }

    #[test]
    fn highlighting_marks_ambiguity_and_the_qualified_target() {
        let surface = Arc::new(RwLock::new(surface_with_colliding_wait()));
        let highlighter = ReplHighlighter::new(surface, Arc::new(RwLock::new(Aliases::default())));
        assert_eq!(
            highlighter.command_style("wait"),
            Style::new().fg(Color::Yellow).bold()
        );
        assert_eq!(
            highlighter.qualified_name_style("tool", "wait"),
            Style::new().fg(Color::Green).bold()
        );
        assert_eq!(
            highlighter.qualified_name_style("builtin", "wait"),
            Style::new().fg(Color::Cyan).bold()
        );
    }
}
