//! Quote-aware command-line tokenization for interactive and `--exec` input.
//!
//! This is intentionally smaller than a shell parser: quotes group one
//! argument, backslashes escape the next character outside single quotes,
//! and a top-level trailing `&` selects task-augmented execution. JSON object
//! and array literals remain byte-for-byte intact so raw `call` arguments and
//! schema-coerced object values do not lose their JSON quotes.

/// A tokenized REPL command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCommand {
    /// Command name followed by its arguments.
    pub words: Vec<String>,
    /// Whether an unquoted trailing `&` requested task-augmented execution.
    pub background: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Quote {
    Single,
    Double,
}

#[derive(Debug)]
struct Word {
    text: String,
    protected: bool,
}

/// Messages for input that is unfinished rather than wrong.
///
/// The distinction is what lets the editor keep reading instead of
/// rejecting a half-typed line: a missing closing brace may still arrive,
/// while a mismatched one never becomes valid. [`is_incomplete`] is the
/// only reader, and the tests below pin each condition to it.
const INCOMPLETE_TRAILING_ESCAPE: &str = "trailing backslash escapes no character";
const INCOMPLETE_JSON_STRING: &str = "unterminated string in JSON argument";
const INCOMPLETE_QUOTE: &str = "unmatched";
const INCOMPLETE_JSON_ARGUMENT: &str = "unclosed JSON argument";

/// Whether the line is merely unfinished, so more input could complete it.
///
/// Drives multiline editing: pasting a pretty-printed JSON body arrives one
/// line at a time, and submitting the first line alone would only produce an
/// error about a brace the user is about to type.
pub fn is_incomplete(line: &str) -> bool {
    match parse(line) {
        Ok(_) => false,
        Err(message) => {
            message.starts_with(INCOMPLETE_QUOTE)
                || message.starts_with(INCOMPLETE_JSON_ARGUMENT)
                || message == INCOMPLETE_TRAILING_ESCAPE
                || message == INCOMPLETE_JSON_STRING
        }
    }
}

/// Split one command line without losing quoted whitespace or JSON syntax.
pub fn parse(line: &str) -> Result<ParsedCommand, String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut word_started = false;
    let mut protected = false;
    let mut quote = None;
    let mut escaped = false;
    let mut json_stack = Vec::new();
    let mut json_string = false;
    let mut json_escaped = false;

    for ch in line.chars() {
        if !json_stack.is_empty() {
            current.push(ch);
            word_started = true;
            if json_string {
                if json_escaped {
                    json_escaped = false;
                } else if ch == '\\' {
                    json_escaped = true;
                } else if ch == '"' {
                    json_string = false;
                }
                continue;
            }
            match ch {
                '"' => json_string = true,
                '{' => json_stack.push('}'),
                '[' => json_stack.push(']'),
                '}' | ']' => {
                    let expected = json_stack.pop().expect("non-empty JSON stack");
                    if ch != expected {
                        return Err(format!(
                            "mismatched JSON delimiter: expected `{expected}`, found `{ch}`"
                        ));
                    }
                }
                _ => {}
            }
            continue;
        }

        if escaped {
            current.push(ch);
            word_started = true;
            protected = true;
            escaped = false;
            continue;
        }

        match quote {
            Some(Quote::Single) => {
                if ch == '\'' {
                    quote = None;
                } else {
                    current.push(ch);
                }
                word_started = true;
                protected = true;
            }
            Some(Quote::Double) => {
                if ch == '"' {
                    quote = None;
                } else if ch == '\\' {
                    escaped = true;
                } else {
                    current.push(ch);
                }
                word_started = true;
                protected = true;
            }
            None => match ch {
                '\\' => {
                    escaped = true;
                    word_started = true;
                    protected = true;
                }
                '\'' => {
                    quote = Some(Quote::Single);
                    word_started = true;
                    protected = true;
                }
                '"' => {
                    quote = Some(Quote::Double);
                    word_started = true;
                    protected = true;
                }
                '{' | '[' if current.is_empty() || current.ends_with('=') => {
                    current.push(ch);
                    word_started = true;
                    json_stack.push(if ch == '{' { '}' } else { ']' });
                }
                c if c.is_whitespace() => {
                    finish_word(&mut words, &mut current, &mut word_started, &mut protected);
                }
                _ => {
                    current.push(ch);
                    word_started = true;
                }
            },
        }
    }

    if escaped {
        return Err(INCOMPLETE_TRAILING_ESCAPE.to_string());
    }
    if let Some(quote) = quote {
        let name = match quote {
            Quote::Single => "single",
            Quote::Double => "double",
        };
        return Err(format!("{INCOMPLETE_QUOTE} {name} quote"));
    }
    if json_string {
        return Err(INCOMPLETE_JSON_STRING.to_string());
    }
    if let Some(expected) = json_stack.last() {
        return Err(format!("{INCOMPLETE_JSON_ARGUMENT}: expected `{expected}`"));
    }
    finish_word(&mut words, &mut current, &mut word_started, &mut protected);

    let background = words
        .last()
        .is_some_and(|word| word.text == "&" && !word.protected);
    if background {
        words.pop();
    }

    Ok(ParsedCommand {
        words: words.into_iter().map(|word| word.text).collect(),
        background,
    })
}

fn finish_word(
    words: &mut Vec<Word>,
    current: &mut String,
    word_started: &mut bool,
    protected: &mut bool,
) {
    if *word_started {
        words.push(Word {
            text: std::mem::take(current),
            protected: *protected,
        });
    }
    *word_started = false;
    *protected = false;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::property::{COMMAND_REGRESSIONS, GENERATED_CASES, Generator};

    fn quote_word(word: &str) -> String {
        let mut quoted = String::from("\"");
        for character in word.chars() {
            if matches!(character, '\\' | '"') {
                quoted.push('\\');
            }
            quoted.push(character);
        }
        quoted.push('"');
        quoted
    }

    fn canonical(parsed: &ParsedCommand) -> String {
        let mut line = parsed
            .words
            .iter()
            .map(|word| quote_word(word))
            .collect::<Vec<_>>()
            .join(" ");
        if parsed.background {
            line.push_str(" &");
        }
        line
    }

    fn assert_command_property(line: &str) {
        match parse(line) {
            Ok(parsed) => {
                let reparsed = parse(&canonical(&parsed))
                    .unwrap_or_else(|error| panic!("canonical form failed: {error}; {line:?}"));
                assert_eq!(
                    reparsed, parsed,
                    "accepted input did not round-trip: {line:?}"
                );
            }
            Err(error) => {
                assert!(!error.is_empty(), "rejection must explain itself: {line:?}");
            }
        }
    }

    #[test]
    fn property_command_parser_round_trips_or_rejects_whole_input() {
        for line in COMMAND_REGRESSIONS {
            assert_command_property(line);
        }

        let mut generator = Generator::new(0x01);
        for _ in 0..GENERATED_CASES {
            let line = generator.text(128);
            assert_command_property(&line);
        }
    }

    #[test]
    fn unfinished_input_asks_for_more() {
        // Each of these could still become valid with more typing, which is
        // exactly the pasted-JSON case.
        assert!(is_incomplete(r#"call echo {"#));
        assert!(is_incomplete(r#"call echo {"message": "#));
        assert!(is_incomplete(r#"call echo {"message": "hi"#));
        assert!(is_incomplete("tool a=[1,"));
        assert!(is_incomplete(r#"tool a="unterminated"#));
        assert!(is_incomplete("tool a='unterminated"));
        assert!(is_incomplete("tool note=two\\"));
    }

    #[test]
    fn finished_or_impossible_input_is_submitted() {
        assert!(!is_incomplete(""));
        assert!(!is_incomplete("tools"));
        assert!(!is_incomplete(r#"call echo {"message": "hi"}"#));
        assert!(!is_incomplete("tool a=[1, 2]"));
        // A mismatched delimiter never becomes valid, so submit it and let
        // the parser report it rather than trapping the editor.
        assert!(!is_incomplete("tool a={1]"));
    }

    #[test]
    fn a_multiline_json_body_parses_as_one_command() {
        let parsed = parse("call echo {\n  \"message\": \"hello world\"\n}").unwrap();
        assert_eq!(parsed.words[0], "call");
        assert_eq!(parsed.words[1], "echo");
        // The JSON keeps its own whitespace, so it stays valid JSON.
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&parsed.words[2]).unwrap(),
            serde_json::json!({"message": "hello world"})
        );
    }

    /// The splitter, over the shapes a `key=value` argument actually takes.
    ///
    /// Table-driven because the failure mode is silent: a value split at a
    /// space still produces a valid call, and the mistake surfaces later as a
    /// confusing result rather than an error (#48).
    #[test]
    fn quoted_values_survive_splitting() {
        let cases: &[(&str, &[&str])] = &[
            // Bare values, the easy case.
            ("tool a=plain", &["tool", "a=plain"]),
            ("tool a=1 b=2", &["tool", "a=1", "b=2"]),
            // Double quotes group spaces and are removed.
            (r#"tool a="two words""#, &["tool", "a=two words"]),
            (
                r#"tool a="two words" b=single"#,
                &["tool", "a=two words", "b=single"],
            ),
            // Single quotes do the same, and take double quotes literally.
            ("tool a='two words'", &["tool", "a=two words"]),
            (r#"tool a='say "hi"'"#, &["tool", r#"a=say "hi""#]),
            // Escapes: a quote inside double quotes, and a bare space.
            (r#"tool a="say \"hi\"""#, &["tool", r#"a=say "hi""#]),
            (r"tool a=two\ words", &["tool", "a=two words"]),
            // An empty value is a value, not an absent argument.
            (r#"tool a="""#, &["tool", "a="]),
            // Whitespace inside quotes is part of the value.
            (r#"tool a="  padded  ""#, &["tool", "a=  padded  "]),
            // Characters the splitter must not treat as syntax.
            (r#"tool a="k=v""#, &["tool", "a=k=v"]),
            (r#"tool a="trailing &""#, &["tool", "a=trailing &"]),
            (r#"tool a="a | b""#, &["tool", "a=a | b"]),
            // Quotes opening mid-value, not right after `=`.
            (r#"tool a=pre"quoted tail""#, &["tool", "a=prequoted tail"]),
            // Several quoted arguments in one line.
            (
                r#"tool a="one two" b="three four""#,
                &["tool", "a=one two", "b=three four"],
            ),
        ];
        for (line, expected) in cases {
            let parsed = parse(line).unwrap_or_else(|e| panic!("{line:?} failed to parse: {e}"));
            assert_eq!(parsed.words, *expected, "{line:?}");
            assert!(!parsed.background, "{line:?} is not a background call");
        }
    }

    #[test]
    fn quotes_group_whitespace_and_are_removed() {
        let parsed = parse(r#"tool a="hello world" b='single value'"#).unwrap();
        assert_eq!(parsed.words, ["tool", "a=hello world", "b=single value"]);
        assert!(!parsed.background);
    }

    #[test]
    fn escapes_quotes_backslashes_and_whitespace() {
        let parsed = parse(r#"tool a="say \"hi\"" path='C:\tmp' note=two\ words"#).unwrap();
        assert_eq!(
            parsed.words,
            ["tool", "a=say \"hi\"", "path=C:\\tmp", "note=two words"]
        );
    }

    #[test]
    fn json_objects_and_arrays_keep_their_quotes_and_spaces() {
        let parsed = parse(
            r#"call run.start {"instruction": "Reply with exactly hello", "items": [1, 2]} &"#,
        )
        .unwrap();
        assert_eq!(
            parsed.words,
            [
                "call",
                "run.start",
                r#"{"instruction": "Reply with exactly hello", "items": [1, 2]}"#,
            ]
        );
        assert!(parsed.background);
    }

    #[test]
    fn only_a_plain_trailing_ampersand_backgrounds() {
        assert!(parse("tool a=1 &").unwrap().background);
        assert!(!parse(r#"tool value="&""#).unwrap().background);
        assert_eq!(
            parse(r#"tool value="&""#).unwrap().words,
            ["tool", "value=&"]
        );
        assert_eq!(
            parse(r#"tool value=\&"#).unwrap().words,
            ["tool", "value=&"]
        );
    }

    #[test]
    fn malformed_quotes_and_escapes_fail_locally() {
        assert_eq!(
            parse(r#"tool a="unfinished"#).unwrap_err(),
            "unmatched double quote"
        );
        assert_eq!(
            parse("tool a=unfinished\\").unwrap_err(),
            "trailing backslash escapes no character"
        );
    }

    #[test]
    fn empty_and_unquoted_arguments_remain_compatible() {
        assert_eq!(
            parse("tool a=1 b=true").unwrap().words,
            ["tool", "a=1", "b=true"]
        );
        assert_eq!(parse(r#"tool empty="""#).unwrap().words, ["tool", "empty="]);
    }
}
