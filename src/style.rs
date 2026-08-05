//! Output styling: color gating, a JSON syntax colorizer, a light
//! markdown renderer, and status-line helpers.
//!
//! All color degrades to plain text when `NO_COLOR` is set or stdout is
//! not a terminal, so piped output stays clean.

use std::borrow::Cow;
use std::io::IsTerminal;
use std::iter::Peekable;
use std::str::Chars;
use std::sync::OnceLock;

use nu_ansi_term::{Color, Style};
use tower_mcp::protocol::TaskStatus;

/// When to emit ANSI colors.
#[derive(Clone, Copy, Debug, Default, clap::ValueEnum)]
pub enum ColorMode {
    /// Color when stdout is a tty and `NO_COLOR` is unset.
    #[default]
    Auto,
    /// Always color, even when piped.
    Always,
    /// Never color.
    Never,
}

static ENABLED: OnceLock<bool> = OnceLock::new();

/// Set the color mode once, before any output. `Always` overrides
/// `NO_COLOR` (an explicit request wins, matching cargo's behavior).
pub fn init(mode: ColorMode) {
    let _ = ENABLED.set(match mode {
        ColorMode::Auto => auto_detect(),
        ColorMode::Always => true,
        ColorMode::Never => false,
    });
}

fn auto_detect() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
}

/// Whether ANSI styling is active.
pub fn colors_enabled() -> bool {
    *ENABLED.get_or_init(auto_detect)
}

/// The visible stand-in for a stripped control character or sequence.
const REPLACEMENT: char = '\u{FFFD}';

/// Neutralize terminal control sequences in server-controlled text.
///
/// The server names its tools, writes their descriptions, and produces
/// every result and notification the REPL renders, so any of those
/// strings can carry escape sequences that reprogram the terminal:
/// OSC 52 writes the clipboard, CSI movement plus `\r` overwrites lines
/// already printed, `ESC[8m` conceals text. This is the one boundary
/// where those bytes are stopped.
///
/// `\n` and `\t` pass through and `\r\n` normalizes to `\n`. Every other
/// C0/C1 control is replaced with U+FFFD, and an ESC-introduced sequence
/// (CSI, OSC, DCS, SOS, PM, APC) is consumed as a unit and replaced with
/// a single U+FFFD, so a hostile payload shows up as visible replacement
/// characters instead of vanishing or executing.
pub fn sanitize(text: &str) -> Cow<'_, str> {
    if !text
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\n' | '\t' => out.push(c),
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                    out.push('\n');
                } else {
                    out.push(REPLACEMENT);
                }
            }
            '\u{1b}' => {
                consume_escape_sequence(&mut chars);
                out.push(REPLACEMENT);
            }
            // 8-bit CSI and OSC introducers get the same treatment as
            // their ESC-prefixed forms so the sequence body is not left
            // behind as stray text.
            '\u{9b}' => {
                consume_csi_body(&mut chars);
                out.push(REPLACEMENT);
            }
            '\u{9d}' => {
                consume_string_body(&mut chars);
                out.push(REPLACEMENT);
            }
            c if c.is_control() => out.push(REPLACEMENT),
            c => out.push(c),
        }
    }
    Cow::Owned(out)
}

fn consume_escape_sequence(chars: &mut Peekable<Chars>) {
    match chars.peek() {
        Some('[') => {
            chars.next();
            consume_csi_body(chars);
        }
        Some(']') | Some('P') | Some('X') | Some('^') | Some('_') => {
            chars.next();
            consume_string_body(chars);
        }
        // Any other two-character sequence (charset selection, keypad
        // modes, ...): drop the byte after ESC.
        Some(_) => {
            chars.next();
        }
        None => {}
    }
}

/// CSI: parameter/intermediate bytes 0x20-0x3F, then one final byte
/// 0x40-0x7E. Anything out of range means a malformed sequence; stop so
/// ordinary text is not swallowed.
fn consume_csi_body(chars: &mut Peekable<Chars>) {
    while let Some(&c) = chars.peek() {
        if ('\u{40}'..='\u{7e}').contains(&c) {
            chars.next();
            break;
        }
        if !('\u{20}'..='\u{3f}').contains(&c) {
            break;
        }
        chars.next();
    }
}

/// OSC/DCS/SOS/PM/APC payload: runs to BEL, ST (`ESC \`), or the 8-bit
/// ST. An unterminated payload consumes the rest of the string, which is
/// the safe direction: the payload was authored to be interpreted, not
/// read.
fn consume_string_body(chars: &mut Peekable<Chars>) {
    while let Some(c) = chars.next() {
        match c {
            '\u{07}' | '\u{9c}' => break,
            '\u{1b}' => {
                if chars.peek() == Some(&'\\') {
                    chars.next();
                }
                break;
            }
            _ => {}
        }
    }
}

/// Apply a style if colors are enabled, otherwise return the text as-is.
pub fn paint(style: Style, text: &str) -> String {
    if colors_enabled() {
        style.paint(text).to_string()
    } else {
        text.to_string()
    }
}

/// How many terminal columns a string occupies.
///
/// Not its byte length and not its character count: a CJK ideograph is two
/// columns wide, a combining mark is zero, and the escape sequences `paint`
/// adds are not drawn at all.
pub fn display_width(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
}

/// A styled left-aligned column of `width` visible columns.
///
/// `{:24}` cannot do this once a value is styled: Rust pads by character
/// count, and the nine bytes of `\e[32m...\e[0m` are charged against the
/// budget, so a colored column silently shrinks to fifteen visible columns
/// and rows stop lining up. Pad against the unstyled text instead, and by
/// what it actually occupies on screen.
///
/// The text is expected to be [`sanitize`]d already: the width of a control
/// character is not a meaningful number.
pub fn column(style: Style, text: &str, width: usize) -> String {
    let padding = width.saturating_sub(display_width(text));
    format!("{}{}", paint(style, text), " ".repeat(padding))
}

/// A `[label]` tag with dim brackets and a styled label.
pub fn tag(style: Style, label: &str) -> String {
    format!(
        "{}{}{}",
        paint(Style::new().dimmed(), "["),
        paint(style, label),
        paint(Style::new().dimmed(), "]")
    )
}

/// Style for a task status: working=yellow, completed=green,
/// failed/cancelled=red.
pub fn task_status_style(status: TaskStatus) -> Style {
    match status {
        TaskStatus::Working => Style::new().fg(Color::Yellow),
        TaskStatus::InputRequired => Style::new().fg(Color::Purple),
        TaskStatus::Completed => Style::new().fg(Color::Green),
        TaskStatus::Failed | TaskStatus::Cancelled => Style::new().fg(Color::Red),
        _ => Style::new(),
    }
}

/// Style an error line prefix.
pub fn error_prefix() -> String {
    paint(Style::new().fg(Color::Red).bold(), "error")
}

// ---------------------------------------------------------------------------
// JSON colorizer
// ---------------------------------------------------------------------------

/// Pretty-print a JSON value with syntax colors (2-space indent, same
/// shape as `serde_json::to_string_pretty`).
pub fn json_pretty(value: &serde_json::Value) -> String {
    let mut out = String::new();
    write_json(&mut out, value, 0);
    out
}

fn write_json(out: &mut String, value: &serde_json::Value, indent: usize) {
    let pad = "  ".repeat(indent);
    let inner_pad = "  ".repeat(indent + 1);
    match value {
        serde_json::Value::Null => out.push_str(&paint(Style::new().fg(Color::Purple), "null")),
        serde_json::Value::Bool(b) => {
            out.push_str(&paint(
                Style::new().fg(Color::Purple),
                if *b { "true" } else { "false" },
            ));
        }
        serde_json::Value::Number(n) => {
            out.push_str(&paint(Style::new().fg(Color::Yellow), &n.to_string()));
        }
        serde_json::Value::String(s) => {
            let quoted = serde_json::to_string(s).unwrap_or_else(|_| format!("{s:?}"));
            out.push_str(&paint(Style::new().fg(Color::Green), &quoted));
        }
        serde_json::Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push_str("[\n");
            for (i, item) in items.iter().enumerate() {
                out.push_str(&inner_pad);
                write_json(out, item, indent + 1);
                if i + 1 < items.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&pad);
            out.push(']');
        }
        serde_json::Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push_str("{\n");
            for (i, (key, val)) in map.iter().enumerate() {
                let quoted = serde_json::to_string(key).unwrap_or_else(|_| format!("{key:?}"));
                out.push_str(&inner_pad);
                out.push_str(&paint(Style::new().fg(Color::Cyan), &quoted));
                out.push_str(": ");
                write_json(out, val, indent + 1);
                if i + 1 < map.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&pad);
            out.push('}');
        }
    }
}

// ---------------------------------------------------------------------------
// Markdown rendering
// ---------------------------------------------------------------------------

/// Heuristic: does this text look like markdown worth rendering?
/// Strong signals (headings, fences) decide immediately; weak signals
/// (bullets, inline code, bold) count too.
pub fn looks_like_markdown(text: &str) -> bool {
    let mut weak_signal = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || heading_level(trimmed).is_some() {
            return true;
        }
        if trimmed.starts_with("- ")
            || trimmed.starts_with("* ")
            || trimmed.contains("**")
            || trimmed.chars().filter(|c| *c == '`').count() >= 2
        {
            weak_signal = true;
        }
    }
    weak_signal
}

fn heading_level(line: &str) -> Option<usize> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&hashes) && line[hashes..].starts_with(' ') {
        Some(hashes)
    } else {
        None
    }
}

/// Render markdown-ish text for the terminal: headings bold, fenced code
/// dimmed, inline code and bold spans styled, list bullets colored.
pub fn render_markdown(text: &str) -> String {
    // Callers pass server-authored text; neutralize control sequences
    // here too so no rendering path depends on the caller remembering.
    let text = sanitize(text);
    let mut out = String::new();
    let mut in_fence = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            out.push_str(&paint(Style::new().dimmed(), line));
            out.push('\n');
            continue;
        }
        if in_fence {
            out.push_str(&paint(Style::new().dimmed(), line));
            out.push('\n');
            continue;
        }
        if let Some(level) = heading_level(trimmed) {
            let body = trimmed[level..].trim_start();
            let style = if level <= 2 {
                Style::new().bold().underline()
            } else {
                Style::new().bold()
            };
            out.push_str(&paint(style, body));
            out.push('\n');
            continue;
        }
        // List bullets: color the marker, render the rest inline.
        let indent_len = line.len() - trimmed.len();
        if let Some(rest) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            out.push_str(&line[..indent_len]);
            out.push_str(&paint(Style::new().fg(Color::Cyan), "•"));
            out.push(' ');
            out.push_str(&render_inline(rest));
            out.push('\n');
            continue;
        }
        out.push_str(&line[..indent_len]);
        out.push_str(&render_inline(trimmed));
        out.push('\n');
    }
    // lines() drops a trailing newline; the callers print with println.
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Inline spans: `` `code` `` and `**bold**` (markers stripped).
fn render_inline(line: &str) -> String {
    // A span is (start, marker, style); pick whichever marker comes first,
    // consume it, repeat.
    let mut out = String::new();
    let mut rest = line;
    loop {
        let code = rest.find('`').map(|i| (i, "`"));
        let bold = rest.find("**").map(|i| (i, "**"));
        let next = match (code, bold) {
            (Some(c), Some(b)) => Some(if c.0 < b.0 { c } else { b }),
            (Some(c), None) => Some(c),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        };
        let Some((start, marker)) = next else {
            out.push_str(rest);
            return out;
        };
        let body_start = start + marker.len();
        let Some(len) = rest[body_start..].find(marker) else {
            // Unterminated marker: emit the rest verbatim.
            out.push_str(rest);
            return out;
        };
        let style = if marker == "`" {
            Style::new().fg(Color::Purple)
        } else {
            Style::new().bold()
        };
        out.push_str(&rest[..start]);
        out.push_str(&paint(style, &rest[body_start..body_start + len]));
        rest = &rest[body_start + len + marker.len()..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drop ANSI sequences so a test can measure what a terminal draws.
    fn visible(text: &str) -> String {
        let mut out = String::new();
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '\u{1b}' {
                out.push(c);
                continue;
            }
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        break;
                    }
                }
            }
        }
        out
    }

    #[test]
    fn a_column_is_the_same_width_painted_or_not() {
        // The bug this replaces: `{:24}` pads the *styled* string, so the
        // escape bytes eat the budget and a colored row stops lining up
        // with an uncolored one.
        let plain = column(Style::new(), "add", 24);
        let painted = column(Style::new().fg(Color::Green).bold(), "add", 24);
        assert_eq!(display_width(&visible(&plain)), 24);
        assert_eq!(display_width(&visible(&painted)), 24);
    }

    #[test]
    fn a_column_is_measured_in_terminal_columns() {
        // Two-column ideographs, and a combining mark that occupies none.
        assert_eq!(display_width("名前"), 4);
        assert_eq!(display_width("e\u{301}"), 1);
        assert_eq!(
            display_width(&visible(&column(Style::new(), "名前", 10))),
            10
        );
        assert_eq!(
            display_width(&visible(&column(Style::new(), "e\u{301}", 10))),
            10
        );
    }

    #[test]
    fn an_oversized_value_is_not_truncated() {
        // Overflowing one row is better than hiding part of a tool's name.
        let long = "a".repeat(40);
        let out = visible(&column(Style::new(), &long, 24));
        assert_eq!(out, long);
    }

    #[test]
    fn plain_text_passes_through_borrowed() {
        let input = "get_downloads  Download statistics, hourly\nsecond line\ttabbed";
        assert!(matches!(sanitize(input), Cow::Borrowed(_)));
    }

    #[test]
    fn osc52_clipboard_write_is_neutralized() {
        // OSC 52 sets the clipboard; the base64 payload must not survive
        // in interpretable form, and the BEL terminator must be consumed.
        let hostile = "before\u{1b}]52;c;Y3VybCBldmlsIHwgc2g=\u{07}after";
        assert_eq!(sanitize(hostile), "before\u{FFFD}after");
    }

    #[test]
    fn osc_with_st_terminator_is_consumed_as_a_unit() {
        let hostile = "a\u{1b}]0;owned\u{1b}\\b";
        assert_eq!(sanitize(hostile), "a\u{FFFD}b");
    }

    #[test]
    fn csi_erase_and_carriage_return_cannot_rewrite_a_line() {
        let hostile = "destructive\r\u{1b}[2Kread-only";
        assert_eq!(sanitize(hostile), "destructive\u{FFFD}\u{FFFD}read-only");
    }

    #[test]
    fn conceal_sequence_is_visible_not_hiding() {
        let hostile = "shown\u{1b}[8mhidden\u{1b}[0m";
        assert_eq!(sanitize(hostile), "shown\u{FFFD}hidden\u{FFFD}");
    }

    #[test]
    fn raw_c1_bytes_are_replaced() {
        // U+009B is the 8-bit CSI: the parameter bytes after it belong to
        // the sequence and must go with it.
        assert_eq!(sanitize("a\u{9b}31mb"), "a\u{FFFD}b");
        assert_eq!(sanitize("a\u{85}b"), "a\u{FFFD}b");
    }

    #[test]
    fn dcs_payload_is_consumed() {
        let hostile = "x\u{1b}Pq#0;2;0;0;0#0~~@@\u{1b}\\y";
        assert_eq!(sanitize(hostile), "x\u{FFFD}y");
    }

    #[test]
    fn unterminated_osc_swallows_the_tail() {
        // The payload was authored for the terminal, not the reader;
        // losing it is the safe direction.
        assert_eq!(sanitize("a\u{1b}]52;c;steal"), "a\u{FFFD}");
    }

    #[test]
    fn crlf_normalizes_and_lone_cr_is_replaced() {
        assert_eq!(sanitize("one\r\ntwo"), "one\ntwo");
        assert_eq!(sanitize("one\rtwo"), "one\u{FFFD}two");
    }

    #[test]
    fn newlines_and_tabs_survive() {
        assert_eq!(sanitize("a\u{1b}[31mb\nc\td"), "a\u{FFFD}b\nc\td");
    }

    #[test]
    fn markdown_rendering_neutralizes_hostile_input() {
        // Our own styling may add escapes when colors are on; the
        // property is that the server's sequences do not survive.
        let hostile = "# title\u{1b}]52;c;evil\u{07}\n- item\u{1b}[2K";
        let rendered = render_markdown(hostile);
        assert!(!rendered.contains("]52;"));
        assert!(!rendered.contains("[2K"));
        assert!(rendered.contains('\u{FFFD}'));
    }
}
