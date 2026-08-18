//! Terminal elicitation: when a server-side tool calls `elicitation/create`,
//! prompt the user for the requested fields on stdin.
//!
//! This works because during a foreground tool call the readline thread is
//! parked on the ack channel, not reading stdin, so plain blocking reads
//! are safe. If the editor currently owns the terminal (a background task
//! elicited while the user sits at the prompt), the request is declined
//! rather than fighting reedline for raw-mode stdin. That parked state is
//! also why a form field's answer can be read through its own `Reedline`
//! instance instead of a raw stdin read: the main prompt's editor is idle
//! for the same reason, so a second one reading the same terminal is not a
//! conflict, and it is what lets Tab complete a field to what its schema
//! already told the operator to type. That editor is only built when stdin
//! is a real terminal; piped stdin (scripts, `-e`, the test suite) keeps
//! reading raw lines, which is also what `--elicitation decline` and every
//! other prompt here still do.
//!
//! Everything shown here is written by the server: the message, the field
//! names, their descriptions, and any URL. A form is therefore a phishing
//! surface, so requests carry a line naming which server asked, fields whose
//! names look like credentials are called out before they are answered, and
//! `--elicitation decline` refuses the whole mechanism. A field's answer
//! never reaches command history either way: the raw-stdin path never
//! touches reedline's history, and the field's own `Reedline` is built with
//! `NoHistory`, a backend that discards everything it is handed.

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use nu_ansi_term::{Color, Style};
use reedline::{
    ColumnarMenu, DefaultCompleter, Emacs, History, HistoryItem, HistoryItemId, HistorySessionId,
    KeyCode, KeyModifiers, MenuBuilder, Prompt, PromptEditMode, PromptHistorySearch, Reedline,
    ReedlineError, ReedlineErrorVariants, ReedlineEvent, ReedlineMenu, SearchQuery, Signal,
    default_emacs_keybindings,
};
use tower_mcp::client::{ClientHandler, NotificationHandler, ServerNotification};
use tower_mcp::error::JsonRpcError;
use tower_mcp::protocol::{
    CreateMessageParams, CreateMessageResult, ElicitAction, ElicitFieldValue, ElicitFormParams,
    ElicitRequestParams, ElicitResult, PrimitiveSchemaDefinition,
};

use crate::output::AsyncOutput;
use crate::sampling::{self, SamplingMode};
use crate::style::{self, paint, tag};
use crate::untrusted::sanitize;

/// How to answer `elicitation/create`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum ElicitationMode {
    /// Show the request and read the answers on stdin.
    #[default]
    Prompt,
    /// Refuse every request.
    Decline,
}

impl ElicitationMode {
    /// The label `info` prints.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Prompt => "prompt",
            Self::Decline => "decline",
        }
    }
}

/// Resolve the effective mode: `--elicitation` when given, otherwise
/// `prompt` interactively and `decline` under `-e`, where there is nobody to
/// answer and a blocked read would hang the script. Same reasoning as
/// sampling.
pub fn resolve(flag: Option<ElicitationMode>, one_shot: bool) -> ElicitationMode {
    match flag {
        Some(mode) => mode,
        None if one_shot => ElicitationMode::Decline,
        None => ElicitationMode::Prompt,
    }
}

static MODE: std::sync::OnceLock<ElicitationMode> = std::sync::OnceLock::new();

/// Record the resolved mode. Called once, from `main`.
pub fn init(mode: ElicitationMode) {
    let _ = MODE.set(mode);
}

/// The mode in effect.
pub fn mode() -> ElicitationMode {
    *MODE.get().unwrap_or(&ElicitationMode::Prompt)
}

/// Which server the connection reached, for the provenance line on a
/// request. Shared rather than passed at construction because the handler is
/// built before the handshake that reports the name, and a reconnect can
/// replace it.
pub type ServerLabel = Arc<RwLock<String>>;

/// Client handler that layers terminal elicitation and sampling on top of
/// the notification callbacks.
pub struct ReplClientHandler {
    notifications: NotificationHandler,
    at_prompt: Arc<AtomicBool>,
    server: ServerLabel,
    output: AsyncOutput,
}

impl ReplClientHandler {
    pub fn new(
        notifications: NotificationHandler,
        at_prompt: Arc<AtomicBool>,
        server: ServerLabel,
        output: AsyncOutput,
    ) -> Self {
        Self {
            notifications,
            at_prompt,
            server,
            output,
        }
    }

    /// The name the connected server gave, for the provenance line.
    fn server_name(&self) -> String {
        self.server
            .read()
            .map(|name| name.clone())
            .unwrap_or_default()
    }

    /// Announce a refusal without corrupting the prompt. These fire while
    /// the editor may own the terminal, which is exactly what the external
    /// printer is for.
    fn note(&self, message: String) {
        self.output.line(message);
    }
}

#[async_trait]
impl ClientHandler for ReplClientHandler {
    async fn handle_create_message(
        &self,
        params: CreateMessageParams,
    ) -> Result<CreateMessageResult, JsonRpcError> {
        match sampling::mode() {
            SamplingMode::Decline => Err(sampling::declined("--sampling decline")),
            SamplingMode::Canned => {
                eprintln!(
                    "{} answered with the canned reply",
                    tag(Style::new().fg(Color::Purple), "sampling")
                );
                Ok(sampling::canned(&params))
            }
            SamplingMode::Prompt => {
                if self.at_prompt.load(Ordering::SeqCst) {
                    // Same constraint as a form elicitation: the editor holds
                    // the terminal in raw mode and a second stdin reader
                    // would corrupt it.
                    self.note(format!(
                        "{} declined a completion request from the server (arrived while at \
                         the prompt; run the tool in the foreground to answer it)",
                        tag(Style::new().fg(Color::Purple), "sampling"),
                    ));
                    return Err(sampling::declined(
                        "arrived while the editor held the terminal",
                    ));
                }
                // Blocking stdin reads must leave the async runtime.
                tokio::task::spawn_blocking(move || sampling::prompt(&params))
                    .await
                    .map_err(|e| JsonRpcError::internal_error(e.to_string()))?
            }
        }
    }

    async fn handle_elicit(
        &self,
        params: ElicitRequestParams,
    ) -> Result<ElicitResult, JsonRpcError> {
        if mode() == ElicitationMode::Decline {
            self.note(format!(
                "{} declined a request from the server (--elicitation decline)",
                tag(Style::new().fg(Color::Purple), "elicit"),
            ));
            return Ok(ElicitResult::decline());
        }
        // Both branches read stdin, so both need the editor to not be
        // holding the terminal in raw mode.
        if self.at_prompt.load(Ordering::SeqCst) {
            self.note(format!(
                "{} declined a request from the server (arrived while at the prompt; run the \
                 tool in the foreground to answer it)",
                tag(Style::new().fg(Color::Purple), "elicit"),
            ));
            return Ok(ElicitResult::decline());
        }
        let server = self.server_name();
        match params {
            ElicitRequestParams::Url(url) => {
                // Only ever a link for the operator to follow by hand, so
                // schemes that execute or read local files have no business
                // here even as display text.
                if !is_web_url(&url.url) {
                    self.note(format!(
                        "{} declined a request to open {} (only http and https are shown)",
                        tag(Style::new().fg(Color::Purple), "elicit"),
                        sanitize(&url.url)
                    ));
                    return Ok(ElicitResult::decline());
                }
                let (message, link) = (url.message.clone(), url.url.clone());
                // Accepting says the operator completed an out-of-band flow,
                // so it has to be the operator who says it.
                tokio::task::spawn_blocking(move || confirm_url(&server, &message, &link))
                    .await
                    .map_err(|e| JsonRpcError::internal_error(e.to_string()))
            }
            ElicitRequestParams::Form(form) => {
                // Blocking stdin reads must leave the async runtime.
                tokio::task::spawn_blocking(move || prompt_form(&server, &form))
                    .await
                    .map_err(|e| JsonRpcError::internal_error(e.to_string()))
            }
            _ => Ok(ElicitResult::decline()),
        }
    }

    async fn on_notification(&self, notification: ServerNotification) {
        self.notifications.on_notification(notification).await;
    }
}

/// Answer one elicitation request from a foreground command.
///
/// The handler above refuses while the editor owns the terminal, because a
/// second stdin reader would corrupt raw mode. This is the other door into
/// the same prompts, for a question a task parked earlier: the operator
/// typed the command that asks it, so the editor is parked on the ack and
/// reading stdin is safe.
///
/// Refusals are explained on stderr rather than through the external
/// printer, since nothing else is competing for the terminal here.
pub async fn answer_in_foreground(server: &str, params: ElicitRequestParams) -> ElicitResult {
    if mode() == ElicitationMode::Decline {
        eprintln!(
            "{} declined (--elicitation decline)",
            tag(Style::new().fg(Color::Purple), "elicit")
        );
        return ElicitResult::decline();
    }
    let server = server.to_string();
    let answered = match params {
        ElicitRequestParams::Url(url) => {
            // Same gate as the server-initiated path: terminals linkify what
            // they print, so a scheme that executes or reads local files is
            // refused rather than shown.
            if !is_web_url(&url.url) {
                eprintln!(
                    "{} declined a request to open {} (only http and https are shown)",
                    tag(Style::new().fg(Color::Purple), "elicit"),
                    sanitize(&url.url)
                );
                return ElicitResult::decline();
            }
            let (message, link) = (url.message.clone(), url.url.clone());
            tokio::task::spawn_blocking(move || confirm_url(&server, &message, &link)).await
        }
        ElicitRequestParams::Form(form) => {
            tokio::task::spawn_blocking(move || prompt_form(&server, &form)).await
        }
        _ => return ElicitResult::decline(),
    };
    answered.unwrap_or_else(|_| ElicitResult::decline())
}

/// Whether a URL is an ordinary web link. Anything else (`javascript:`,
/// `file:`, `data:`) is refused rather than displayed, since terminals
/// linkify what they print.
fn is_web_url(url: &str) -> bool {
    let scheme = url
        .split_once("://")
        .map(|(scheme, _)| scheme)
        .unwrap_or("");
    scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")
}

/// The line that says who is asking. The message body is server-authored, so
/// the request is framed as coming from the server rather than from the REPL.
fn provenance(server: &str) -> String {
    let who = if server.is_empty() {
        "the server".to_string()
    } else {
        format!("server {}", sanitize(server))
    };
    format!(
        "{} {who} is asking:",
        tag(Style::new().fg(Color::Purple), "elicit")
    )
}

/// Read one line from stdin, or `None` at EOF.
fn read_line() -> Option<String> {
    let mut buf = String::new();
    let read = {
        let mut lock = std::io::stdin().lock();
        std::io::BufRead::read_line(&mut lock, &mut buf)
    };
    match read {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(buf),
    }
}

/// Ask before reporting that the operator followed a URL flow. Answering
/// yes is a claim about something that happened outside the REPL, so the
/// REPL cannot make it on the operator's behalf.
fn confirm_url(server: &str, message: &str, url: &str) -> ElicitResult {
    eprintln!("{}", provenance(server));
    eprintln!("  {}", sanitize(message));
    eprintln!(
        "  open: {}",
        paint(Style::new().underline(), &sanitize(url))
    );
    eprint!("  confirm you completed this [y/N]> ");
    let _ = std::io::stderr().flush();
    match read_line() {
        Some(answer) if matches!(answer.trim(), "y" | "Y" | "yes" | "Yes") => ElicitResult {
            action: ElicitAction::Accept,
            content: None,
            meta: None,
        },
        Some(_) => ElicitResult::decline(),
        None => ElicitResult::cancel(),
    }
}

/// Menu name for the Tab-triggered completion popup on a field's answer.
/// Separate from `editor.rs`'s constant of the same purpose: a different
/// `Reedline` instance, a different menu.
const FIELD_MENU_NAME: &str = "elicit_completion_menu";

/// A history backend that discards everything it is handed.
///
/// `Reedline::create()` already defaults to an in-memory history nothing
/// outside this process could read, but relying on that default is exactly
/// the kind of thing a later change could quietly undo by adding
/// `.with_history(...)`. Wiring this in explicitly, and pinning it with a
/// test, makes "an elicitation answer never enters history" a decision
/// rather than an accident -- this is the path that reads credentials.
struct NoHistory;

impl History for NoHistory {
    fn save(&mut self, h: HistoryItem) -> reedline::Result<HistoryItem> {
        // Reedline calls this on every accepted line and `.expect()`s the
        // result, so returning the item unmodified -- rather than an error
        // -- is what keeps a field's answer from turning into a panic.
        // Nothing is stored anywhere.
        Ok(h)
    }

    fn load(&self, _id: HistoryItemId) -> reedline::Result<HistoryItem> {
        Err(ReedlineError(
            ReedlineErrorVariants::HistoryFeatureUnsupported {
                history: "elicit::NoHistory",
                feature: "load",
            },
        ))
    }

    fn count(&self, _query: SearchQuery) -> reedline::Result<i64> {
        Ok(0)
    }

    fn search(&self, _query: SearchQuery) -> reedline::Result<Vec<HistoryItem>> {
        Ok(Vec::new())
    }

    fn update(
        &mut self,
        _id: HistoryItemId,
        _updater: &dyn Fn(HistoryItem) -> HistoryItem,
    ) -> reedline::Result<()> {
        Ok(())
    }

    fn clear(&mut self) -> reedline::Result<()> {
        Ok(())
    }

    fn delete(&mut self, _id: HistoryItemId) -> reedline::Result<()> {
        Ok(())
    }

    fn sync(&mut self) -> std::io::Result<()> {
        Ok(())
    }

    fn session(&self) -> Option<HistorySessionId> {
        None
    }
}

/// The prompt for one field's answer: the same `  <name>> ` text the raw
/// stdin path prints, so switching to reedline does not change what is on
/// screen.
struct FieldPrompt {
    left: String,
}

impl FieldPrompt {
    fn new(display_name: &str) -> Self {
        Self {
            left: format!("  {display_name}"),
        }
    }
}

impl Prompt for FieldPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.left)
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
        _history_search: PromptHistorySearch,
    ) -> Cow<'_, str> {
        // Unreachable with `NoHistory` backing the editor -- there is
        // nothing to search -- but the trait still requires an
        // implementation.
        Cow::Borrowed("")
    }
}

/// The words a field's answer completes to: the same choices
/// `accepted_values` hands `reject_answer`, so completion and validation
/// cannot list two different answers for the same field. Booleans complete
/// to `true`/`false` even though `coerce_field` also accepts `y`, `yes`,
/// `on` -- Tab offers the canonical spelling without narrowing what typing
/// the rest by hand still accepts. A field with a default gets it added too,
/// including fields with neither an enum nor a boolean type. A pure function
/// so the word list is testable without a terminal.
fn completion_words(schema: &PrimitiveSchemaDefinition, default: Option<&str>) -> Vec<String> {
    let is_boolean = field_json(schema).get("type").and_then(|t| t.as_str()) == Some("boolean");
    let mut words = if is_boolean {
        vec!["true".to_string(), "false".to_string()]
    } else {
        accepted_values(schema)
            .map(|(choices, _multi)| choices)
            .unwrap_or_default()
    };
    if let Some(d) = default
        && !words.iter().any(|w| w == d)
    {
        words.push(d.to_string());
    }
    words
}

/// Build the `Reedline` instance that reads one field's answer.
///
/// Only called when stdin is a real terminal (`prompt_form` checks once,
/// before the fields loop): a second instance is safe here because the main
/// prompt's editor is idle for the same reason a background elicitation is
/// declined -- see the module doc. Piped stdin keeps reading raw lines
/// instead; a `Reedline` cannot read one (confirmed empirically: it errors
/// immediately rather than blocking, unlike a raw `read_line` on the same
/// pipe).
fn field_editor(schema: &PrimitiveSchemaDefinition, default: Option<&str>) -> Reedline {
    let completer = DefaultCompleter::new_with_wordlen(completion_words(schema, default), 1);
    let menu = ColumnarMenu::default().with_name(FIELD_MENU_NAME);
    let mut keybindings = default_emacs_keybindings();
    keybindings.add_binding(
        KeyModifiers::NONE,
        KeyCode::Tab,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::Menu(FIELD_MENU_NAME.to_string()),
            ReedlineEvent::MenuNext,
        ]),
    );
    Reedline::create()
        .with_history(Box::new(NoHistory))
        .with_completer(Box::new(completer))
        .with_menu(ReedlineMenu::EngineCompleter(Box::new(menu)))
        .with_edit_mode(Box::new(Emacs::new(keybindings)))
        .with_ansi_colors(style::colors_enabled())
}

/// Read one field's raw input: through its `Reedline` when one was built for
/// this field, otherwise a blocking line off stdin as before. `None` means
/// cancel the whole form -- EOF either way, or `Ctrl-C`/`Ctrl-D` at the
/// editor.
fn read_field_answer(editor: Option<&mut Reedline>, display_name: &str) -> Option<String> {
    match editor {
        Some(ed) => {
            let prompt = FieldPrompt::new(display_name);
            match ed.read_line(&prompt) {
                Ok(Signal::Success(line)) => Some(line),
                Ok(_) | Err(_) => None,
            }
        }
        None => {
            eprint!("  {display_name}> ");
            let _ = std::io::stderr().flush();
            let mut buf = String::new();
            let read = {
                let mut lock = std::io::stdin().lock();
                std::io::BufRead::read_line(&mut lock, &mut buf)
            };
            match read {
                Ok(0) | Err(_) => None,
                Ok(_) => Some(buf),
            }
        }
    }
}

/// Prompt for each field of a form elicitation on stdin. EOF at any point
/// cancels. Empty input picks the default when one exists, otherwise skips
/// optional fields.
fn prompt_form(server: &str, form: &ElicitFormParams) -> ElicitResult {
    eprintln!("{}", provenance(server));
    eprintln!("  {}", sanitize(&form.message));
    // A `Reedline` cannot read a pipe (see `field_editor`), so this is
    // checked once rather than per field.
    let interactive = std::io::stdin().is_terminal();
    let mut content: HashMap<String, ElicitFieldValue> = HashMap::new();
    // Declaration order, not alphabetical. The schema is an `IndexMap` and
    // the order is protocol-significant: a server puts the fields in the
    // order it wants them answered, and asking out of order is how an answer
    // ends up in the wrong field.
    for (name, schema) in &form.requested_schema.properties {
        let required = form.requested_schema.required.iter().any(|r| r == name);
        let (ty, detail, default) = describe_field(schema);
        // Field names, descriptions, and defaults are all server-authored.
        let display_name = sanitize(name);
        let mut prompt_line = format!(
            "  {} ({}",
            paint(Style::new().fg(Color::Cyan), &display_name),
            sanitize(&ty)
        );
        if required {
            prompt_line.push_str(", required");
        }
        if let Some(d) = &default {
            prompt_line.push_str(&format!(", default {}", sanitize(d)));
        }
        prompt_line.push(')');
        if let Some(detail) = detail {
            prompt_line.push_str(&format!(
                " {}",
                paint(Style::new().dimmed(), &sanitize(&detail))
            ));
        }
        eprintln!("{prompt_line}");
        // A server can ask for anything it likes, including an API key it
        // has no business holding. Say so before the answer is typed, since
        // afterwards it is too late.
        if crate::wire::looks_like_credential(name) {
            eprintln!(
                "  {} this field name looks like a credential; mcp-repl sends the answer to \
                 the server as typed",
                paint(Style::new().fg(Color::Yellow).bold(), "warning:")
            );
        }
        let mut editor = interactive.then(|| field_editor(schema, default.as_deref()));
        loop {
            let Some(buf) = read_field_answer(editor.as_mut(), &display_name) else {
                return ElicitResult::cancel();
            };
            let raw = buf.trim();
            if raw.is_empty() {
                match (&default, required) {
                    (Some(d), _) => {
                        content.insert(name.clone(), coerce_field(schema, d));
                        break;
                    }
                    (None, false) => break,
                    (None, true) => {
                        eprintln!("  (required)");
                        continue;
                    }
                }
            }
            if let Some(complaint) = reject_answer(schema, raw) {
                eprintln!("{}", sanitize(&complaint));
                continue;
            }
            content.insert(name.clone(), coerce_field(schema, raw));
            break;
        }
    }
    ElicitResult::accept(content)
}

/// Type label, optional detail (description or enum choices), and default
/// value for a field schema.
fn describe_field(schema: &PrimitiveSchemaDefinition) -> (String, Option<String>, Option<String>) {
    let raw = field_json(schema);
    let description = raw
        .get("description")
        .and_then(|d| d.as_str())
        .map(str::to_string);
    let default = raw.get("default").map(render_default);
    // Enum choices are more useful than the word "string": they are the
    // answer, not the shape of it. A multi-select carries them one level
    // down, on the schema for each item.
    let choices = raw.get("enum").or_else(|| raw.pointer("/items/enum"));
    if let Some(values) = choices.and_then(|e| e.as_array()) {
        let choices: Vec<String> = values.iter().map(render_default).collect();
        let label = match raw.get("type").and_then(|t| t.as_str()) {
            Some("array") => format!("any of {}, comma-separated", choices.join("|")),
            _ => format!("one of {}", choices.join("|")),
        };
        return (label, description, default);
    }
    let label = match raw.get("type").and_then(|t| t.as_str()) {
        Some("array") => "comma-separated list".to_string(),
        Some(other) => other.to_string(),
        None => "value".to_string(),
    };
    (label, description, default)
}

/// The values a field will accept, when it names them, and whether it takes
/// several.
///
/// Only an `enum`, which is the one case where the schema states the whole
/// answer and a wrong one cannot be salvaged. Everything else is sent as
/// typed, because mcp-repl is not a JSON Schema validator and should not grow
/// into one.
///
/// Booleans are deliberately excluded even though their values enumerate.
/// `coerce_field` accepts `y`, `yes`, and `on` and converts them, so the
/// generous spellings are a feature of answering a prompt by hand rather than
/// a mistake to correct.
fn accepted_values(schema: &PrimitiveSchemaDefinition) -> Option<(Vec<String>, bool)> {
    let raw = field_json(schema);
    let multi = raw.get("type").and_then(|t| t.as_str()) == Some("array");
    let values = raw
        .get("enum")
        .or_else(|| raw.pointer("/items/enum"))
        .and_then(|e| e.as_array())?;
    Some((values.iter().map(render_default).collect(), multi))
}

/// Why an answer cannot be sent, when the field named what it accepts.
///
/// A prompt exists to give an answer back immediately. Sending a value the
/// schema on screen already rules out, and waiting to see whether the server
/// happens to check, is worse than saying so at the point of typing.
fn reject_answer(schema: &PrimitiveSchemaDefinition, raw: &str) -> Option<String> {
    let (choices, multi) = accepted_values(schema)?;
    let mut unknown: Vec<&str> = Vec::new();
    let answers: Vec<&str> = if multi {
        raw.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect()
    } else {
        vec![raw]
    };
    for answer in answers {
        if !choices.iter().any(|choice| choice == answer) {
            unknown.push(answer);
        }
    }
    if unknown.is_empty() {
        return None;
    }
    Some(format!(
        "  {} is not one of: {}",
        unknown.join(", "),
        choices.join(", ")
    ))
}

/// A field schema as plain JSON.
///
/// Reading the JSON rather than matching the typed
/// `PrimitiveSchemaDefinition` variant: the schema is what the server said,
/// and one shape handles every field kind, including ones the union grows
/// later. It is also what kept booleans and enums working through
/// tower-mcp 0.18, where the union was undiscriminated and every field
/// arrived as `String`.
fn field_json(schema: &PrimitiveSchemaDefinition) -> serde_json::Value {
    serde_json::to_value(schema).unwrap_or_else(|_| serde_json::json!({}))
}

/// A default or enum choice as the operator would type it: a bare string
/// without quotes, everything else as JSON.
fn render_default(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Coerce raw input to the field's declared type, falling back to the raw
/// string when parsing fails (the server validates anyway).
fn coerce_field(schema: &PrimitiveSchemaDefinition, raw: &str) -> ElicitFieldValue {
    // Same reason as `describe_field`: the declared type comes from the JSON,
    // not from which variant serde happened to pick.
    let json = field_json(schema);
    match json.get("type").and_then(|t| t.as_str()) {
        Some("integer") => raw
            .parse::<i64>()
            .map(ElicitFieldValue::Integer)
            .unwrap_or_else(|_| ElicitFieldValue::String(raw.to_string())),
        Some("number") => raw
            .parse::<f64>()
            .map(ElicitFieldValue::Number)
            .unwrap_or_else(|_| ElicitFieldValue::String(raw.to_string())),
        Some("boolean") => match raw.to_ascii_lowercase().as_str() {
            "true" | "yes" | "y" | "1" => ElicitFieldValue::Boolean(true),
            "false" | "no" | "n" | "0" => ElicitFieldValue::Boolean(false),
            _ => ElicitFieldValue::String(raw.to_string()),
        },
        Some("array") => {
            ElicitFieldValue::StringArray(raw.split(',').map(|s| s.trim().to_string()).collect())
        }
        _ => ElicitFieldValue::String(raw.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reedline::{Completer, SearchDirection};

    /// Build a field the way a server does: as JSON on the wire, parsed
    /// into the protocol type. Constructing the type directly would hide
    /// the very deserialization this guards against.
    fn field_from_wire(json: serde_json::Value) -> PrimitiveSchemaDefinition {
        serde_json::from_value(json).expect("field schema")
    }

    #[test]
    fn a_declared_type_survives_the_untagged_union() {
        // Every variant of this union deserializes as `String`, so the label
        // has to come from the JSON rather than from the matched variant.
        let (label, _, _) = describe_field(&field_from_wire(
            serde_json::json!({"type": "boolean", "description": "Stay signed in"}),
        ));
        assert_eq!(label, "boolean");
        let (label, description, default) = describe_field(&field_from_wire(
            serde_json::json!({"type": "integer", "description": "How many", "default": 3}),
        ));
        assert_eq!(label, "integer");
        assert_eq!(description.as_deref(), Some("How many"));
        assert_eq!(default.as_deref(), Some("3"));
    }

    /// The complaint an answer draws, or `None` when it is acceptable.
    fn rejected(field: serde_json::Value, raw: &str) -> Option<String> {
        reject_answer(&field_from_wire(field), raw)
    }

    #[test]
    fn an_answer_outside_the_choices_is_refused_with_the_choices() {
        let field = serde_json::json!({"type": "string", "enum": ["staging", "production"]});
        let complaint = rejected(field.clone(), "stagi").expect("refused");
        assert!(complaint.contains("stagi"), "{complaint}");
        assert!(complaint.contains("staging, production"), "{complaint}");
        assert!(rejected(field, "staging").is_none());
    }

    #[test]
    fn a_boolean_keeps_its_generous_spellings() {
        // `coerce_field` accepts y/yes/on and converts them, so answering a
        // boolean by hand is deliberately forgiving. Checking it against
        // `true`/`false` would reject answers that already work, which is what
        // the elicitation e2e case feeds.
        let field = serde_json::json!({"type": "boolean"});
        for answer in ["y", "yes", "on", "true", "false", "n"] {
            assert!(rejected(field.clone(), answer).is_none(), "{answer}");
        }
    }

    #[test]
    fn a_multi_select_checks_every_element() {
        let field = serde_json::json!({
            "type": "array",
            "items": {"type": "string", "enum": ["a", "b", "c"]},
        });
        assert!(rejected(field.clone(), "a, c").is_none());
        let complaint = rejected(field.clone(), "a, z").expect("refused");
        assert!(complaint.contains('z'), "{complaint}");
        // The complaint names only what was wrong, not the whole answer.
        assert!(!complaint.starts_with("  a,"), "{complaint}");
    }

    #[test]
    fn an_unconstrained_field_accepts_anything() {
        // mcp-repl is not a JSON Schema validator; only fields that state
        // their whole answer are checked.
        assert!(rejected(serde_json::json!({"type": "string"}), "whatever").is_none());
        assert!(rejected(serde_json::json!({"type": "integer"}), "not a number").is_none());
        assert!(rejected(serde_json::json!({}), "").is_none());
    }

    /// The value as it goes back on the wire, which is what the server
    /// actually receives.
    fn coerced(field: serde_json::Value, raw: &str) -> serde_json::Value {
        serde_json::to_value(coerce_field(&field_from_wire(field), raw)).expect("field value")
    }

    #[test]
    fn an_answer_is_coerced_to_the_declared_type() {
        // A server asking for a number must not receive the string "5".
        assert_eq!(coerced(serde_json::json!({"type": "integer"}), "5"), 5);
        assert_eq!(coerced(serde_json::json!({"type": "number"}), "1.5"), 1.5);
        assert_eq!(coerced(serde_json::json!({"type": "boolean"}), "yes"), true);
        assert_eq!(
            coerced(
                serde_json::json!({
                    "type": "array",
                    "items": {"type": "string", "enum": ["a", "b", "c"]},
                }),
                "a, b"
            ),
            serde_json::json!(["a", "b"])
        );
        // Anything that does not parse falls back to the raw text, which the
        // server validates.
        assert_eq!(
            coerced(serde_json::json!({"type": "integer"}), "many"),
            "many"
        );
    }

    #[test]
    fn enum_choices_reach_the_prompt() {
        // The whole point of an enum field: the operator sees the answers.
        let (label, _, _) = describe_field(&field_from_wire(serde_json::json!({
            "type": "string",
            "enum": ["staging", "production"],
        })));
        assert_eq!(label, "one of staging|production");

        let (label, _, _) = describe_field(&field_from_wire(serde_json::json!({
            "type": "array",
            "items": {"type": "string", "enum": ["read", "write"]},
        })));
        assert_eq!(label, "any of read|write, comma-separated");
    }

    #[test]
    fn a_string_field_stays_a_string() {
        assert_eq!(coerced(serde_json::json!({"type": "string"}), "5"), "5");
    }

    #[test]
    fn a_script_declines_elicitation_unless_it_asked_for_it() {
        // Nobody is at the keyboard under -e, so a form would block a run
        // that can never answer it.
        assert_eq!(resolve(None, true), ElicitationMode::Decline);
        assert_eq!(resolve(None, false), ElicitationMode::Prompt);
        // An explicit flag wins in both directions.
        assert_eq!(
            resolve(Some(ElicitationMode::Prompt), true),
            ElicitationMode::Prompt
        );
        assert_eq!(
            resolve(Some(ElicitationMode::Decline), false),
            ElicitationMode::Decline
        );
    }

    #[test]
    fn only_web_links_are_shown() {
        assert!(is_web_url("https://example.com/authorize?x=1"));
        assert!(is_web_url("http://127.0.0.1:8080/cb"));
        assert!(is_web_url("HTTPS://EXAMPLE.COM"));
        // Terminals linkify what gets printed, so these never reach the
        // screen as clickable text.
        assert!(!is_web_url("javascript:alert(1)"));
        assert!(!is_web_url("file:///etc/passwd"));
        assert!(!is_web_url("data:text/html;base64,PHNjcmlwdD4="));
        assert!(!is_web_url("not a url"));
        assert!(!is_web_url(""));
    }

    #[test]
    fn the_provenance_line_names_the_server() {
        let line = provenance("cratesio-mcp");
        assert!(line.contains("cratesio-mcp"));
        assert!(line.contains("is asking"));
        // Before the handshake reports a name there is still a subject.
        assert!(provenance("").contains("the server"));
    }

    #[test]
    fn a_hostile_server_name_cannot_repaint_the_provenance_line() {
        let line = provenance("evil\u{1b}[2K\rmcp-repl");
        assert!(!line.contains('\u{1b}'));
        assert!(line.contains('\u{FFFD}'));
    }

    #[test]
    fn credential_shaped_field_names_are_flagged() {
        for name in [
            "api_key",
            "apiKey",
            "password",
            "github_token",
            "AWS_SECRET_ACCESS_KEY",
            "passphrase",
        ] {
            assert!(
                crate::wire::looks_like_credential(name),
                "{name} should be flagged before the operator types a value"
            );
        }
        for name in ["city", "message", "count", "email"] {
            assert!(!crate::wire::looks_like_credential(name), "{name}");
        }
    }

    #[test]
    fn completion_words_match_what_the_field_accepts() {
        // The same list `reject_answer` checks against, so Tab and
        // validation cannot disagree about a field's enum.
        let field = field_from_wire(serde_json::json!({
            "type": "string",
            "enum": ["staging", "production"],
        }));
        assert_eq!(
            completion_words(&field, None),
            vec!["staging", "production"]
        );

        // A boolean completes to the canonical spelling even though
        // `coerce_field` accepts more.
        let field = field_from_wire(serde_json::json!({"type": "boolean"}));
        assert_eq!(completion_words(&field, None), vec!["true", "false"]);

        // A field with neither still completes to its default.
        let field = field_from_wire(serde_json::json!({"type": "string", "default": "ada"}));
        assert_eq!(completion_words(&field, Some("ada")), vec!["ada"]);

        // A field with neither an enum, a boolean type, nor a default has
        // nothing to offer.
        let field = field_from_wire(serde_json::json!({"type": "string"}));
        assert!(completion_words(&field, None).is_empty());

        // The default is not duplicated when it is already one of the
        // enum's choices.
        let field = field_from_wire(serde_json::json!({
            "type": "string",
            "enum": ["staging", "production"],
            "default": "staging",
        }));
        assert_eq!(
            completion_words(&field, Some("staging")),
            vec!["staging", "production"]
        );
    }

    #[test]
    fn tab_completes_an_enum_field_to_its_values() {
        let field = field_from_wire(serde_json::json!({
            "type": "string",
            "enum": ["staging", "production"],
        }));
        let mut completer = DefaultCompleter::new_with_wordlen(completion_words(&field, None), 1);
        let suggestions: Vec<String> = completer
            .complete("s", 1)
            .into_iter()
            .map(|s| s.value)
            .collect();
        assert_eq!(suggestions, vec!["staging"]);
    }

    #[test]
    fn tab_completes_a_boolean_field_to_true_and_false() {
        let field = field_from_wire(serde_json::json!({"type": "boolean"}));
        let mut completer = DefaultCompleter::new_with_wordlen(completion_words(&field, None), 1);
        let mut suggestions: Vec<String> = completer
            .complete("t", 1)
            .into_iter()
            .map(|s| s.value)
            .collect();
        suggestions.extend(completer.complete("f", 1).into_iter().map(|s| s.value));
        assert!(suggestions.contains(&"true".to_string()), "{suggestions:?}");
        assert!(
            suggestions.contains(&"false".to_string()),
            "{suggestions:?}"
        );
    }

    #[test]
    fn a_field_editor_never_remembers_an_answer() {
        // The exact function `prompt_form` calls, so this pins the
        // production construction rather than `NoHistory` in isolation.
        let field = field_from_wire(serde_json::json!({"type": "string"}));
        let mut editor = field_editor(&field, None);
        editor
            .history_mut()
            .save(HistoryItem::from_command_line("s3cr3t-answer"))
            .expect("a no-op history still reports success");
        assert_eq!(
            editor.history().count_all().expect("count"),
            0,
            "an answer must not be retrievable from the field's history"
        );
    }

    #[test]
    fn no_history_reports_success_but_keeps_nothing() {
        let mut history = NoHistory;
        let saved = history
            .save(HistoryItem::from_command_line("hunter2"))
            .expect("save must not error, or reedline's engine panics");
        assert_eq!(saved.command_line, "hunter2", "the item is handed back");
        assert_eq!(history.count_all().expect("count"), 0);
        assert!(
            history
                .search(SearchQuery::everything(SearchDirection::Forward, None))
                .expect("search")
                .is_empty()
        );
    }
}
