//! Text the server controls, and the one place its control bytes are stopped.
//!
//! This is a safety boundary, not a presentation choice. It lived in `style`
//! beside `paint` and `tag`, which made it look like one: those two add color
//! and a consumer that does not want color simply does not call them. Skipping
//! this one is not a formatting decision, it is a terminal injection.
//!
//! Keeping it here means a caller reaches for `untrusted::sanitize` and says at
//! the call site what kind of text it is holding, and a module that handles
//! server text no longer has to import a rendering module to stay safe.

use std::borrow::Cow;
use std::iter::Peekable;
use std::str::Chars;

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
