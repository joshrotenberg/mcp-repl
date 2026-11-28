//! Schema-aware argument parsing for tool and prompt dispatch.
//!
//! This module owns the boundary between the REPL's `key=value` command
//! language and the typed JSON objects sent over MCP. Keeping it independent
//! from command dispatch makes coercion and rejection testable without a
//! client, session, or terminal.

use std::collections::HashMap;

/// Parse tool arguments, coercing values according to `inputSchema`.
pub(crate) fn parse_kv_args(
    schema: &serde_json::Value,
    tokens: &[&str],
) -> Result<serde_json::Value, String> {
    // A single JSON object literal wins.
    if tokens.len() == 1 && tokens[0].starts_with('{') {
        let value: serde_json::Value = serde_json::from_str(tokens[0])
            .map_err(|error| format!("invalid JSON object argument: {error}"))?;
        if !value.is_object() {
            return Err("the JSON argument must be an object".to_string());
        }
        return Ok(value);
    }
    let mut map = serde_json::Map::new();
    for token in tokens {
        let (key, value) = split_kv_arg(token)?;
        map.insert(key.to_string(), coerce_arg(schema, key, value));
    }
    Ok(serde_json::Value::Object(map))
}

/// Parse prompt arguments without schema coercion.
pub(crate) fn parse_prompt_args(tokens: &[&str]) -> Result<HashMap<String, String>, String> {
    let mut arguments = HashMap::new();
    for token in tokens {
        let (key, value) = split_kv_arg(token)?;
        arguments.insert(key.to_string(), value.to_string());
    }
    Ok(arguments)
}

/// Why a prompt call's argument names do not fit the prompt's declaration.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PromptArgsError {
    pub message: String,
    /// The suggested declared name when exactly one supplied name is
    /// undeclared. With several, each suggestion is in `message` instead.
    pub hint: Option<String>,
}

/// Check parsed prompt arguments against the prompt's declared `arguments`.
///
/// This is the REPL's counterpart of the generated CLI, where clap rejects an
/// unknown flag and a missing required one before anything is sent. An
/// undeclared name is reported ahead of a missing required one, because the
/// usual cause of both is one misspelled name, and the suggestion fixes both.
///
/// A prompt that declares no arguments is not checked at all. tower-mcp reads
/// an omitted `arguments` and `[]` as the same empty list, and the spec does
/// not say what omission means, so a server may still accept arguments it
/// never advertised.
pub(crate) fn check_prompt_args(
    prompt: &tower_mcp::protocol::PromptDefinition,
    supplied: &HashMap<String, String>,
) -> Result<(), PromptArgsError> {
    let declared: Vec<&str> = prompt
        .arguments
        .iter()
        .map(|argument| argument.name.as_str())
        .collect();
    if declared.is_empty() {
        return Ok(());
    }
    let mut unknown: Vec<&str> = supplied
        .keys()
        .map(String::as_str)
        .filter(|key| !declared.contains(key))
        .collect();
    if !unknown.is_empty() {
        unknown.sort_unstable();
        let suggestions: Vec<Option<String>> = unknown
            .iter()
            .map(|key| suggest_argument(key, &declared))
            .collect();
        if let ([key], [hint]) = (unknown.as_slice(), suggestions.as_slice()) {
            let mut message = format!("prompt {:?} has no argument {key:?}", prompt.name);
            if hint.is_none() {
                message.push_str(&format!("; it takes {}", quoted_list(&declared)));
            }
            return Err(PromptArgsError {
                message,
                hint: hint.clone(),
            });
        }
        let listed = unknown
            .iter()
            .zip(&suggestions)
            .map(|(key, hint)| match hint {
                Some(hint) => format!("{key:?} (did you mean {hint:?}?)"),
                None => format!("{key:?}"),
            })
            .collect::<Vec<_>>()
            .join(", ");
        let mut message = format!("prompt {:?} has no arguments {listed}", prompt.name);
        if suggestions.iter().any(Option::is_none) {
            message.push_str(&format!("; it takes {}", quoted_list(&declared)));
        }
        return Err(PromptArgsError {
            message,
            hint: None,
        });
    }
    let missing: Vec<&str> = prompt
        .arguments
        .iter()
        .filter(|argument| argument.required && !supplied.contains_key(&argument.name))
        .map(|argument| argument.name.as_str())
        .collect();
    if !missing.is_empty() {
        return Err(PromptArgsError {
            message: format!(
                "prompt {:?} is missing required {} {}",
                prompt.name,
                if missing.len() == 1 {
                    "argument"
                } else {
                    "arguments"
                },
                quoted_list(&missing)
            ),
            hint: None,
        });
    }
    Ok(())
}

/// The declared argument an undeclared `key` most likely meant.
///
/// Edit distance catches a typo (`nme` for `name`). Failing that, a declared
/// name that contains the key or is contained by it catches a qualified or
/// shortened spelling (`crate_name` for `name`), but only when exactly one
/// declared name qualifies; with two, picking one would be a guess.
fn suggest_argument(key: &str, declared: &[&str]) -> Option<String> {
    if let Some(close) = crate::find::closest(key, declared.iter().copied()) {
        return Some(close);
    }
    let key = key.to_lowercase();
    let mut related = declared.iter().filter(|name| {
        let name = name.to_lowercase();
        !name.is_empty() && (key.contains(&name) || name.contains(&key))
    });
    match (related.next(), related.next()) {
        (Some(only), None) => Some((*only).to_string()),
        _ => None,
    }
}

/// `"a", "b"`, each name quoted the way the rest of the message quotes names.
fn quoted_list(names: &[&str]) -> String {
    names
        .iter()
        .map(|name| format!("{name:?}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Coerce a `key=value` string according to the tool's input schema.
fn coerce_arg(schema: &serde_json::Value, key: &str, raw: &str) -> serde_json::Value {
    let ty = schema
        .get("properties")
        .and_then(|properties| properties.get(key))
        .and_then(|property| {
            property
                .get("type")
                .or_else(|| crate::command_set::resolve_ref(schema, property).get("type"))
        })
        .and_then(|kind| kind.as_str());
    match ty {
        Some("string") => serde_json::Value::String(raw.to_string()),
        Some("integer") => raw
            .parse::<i64>()
            .map(Into::into)
            .unwrap_or_else(|_| serde_json::Value::String(raw.to_string())),
        Some("number") => raw
            .parse::<f64>()
            .ok()
            .and_then(|number| serde_json::Number::from_f64(number).map(serde_json::Value::Number))
            .unwrap_or_else(|| serde_json::Value::String(raw.to_string())),
        Some("boolean") => raw
            .parse::<bool>()
            .map(serde_json::Value::Bool)
            .unwrap_or_else(|_| serde_json::Value::String(raw.to_string())),
        Some("array") | Some("object") => {
            serde_json::from_str(raw).unwrap_or_else(|_| serde_json::Value::String(raw.to_string()))
        }
        _ => {
            // No schema type: accept JSON literals, fall back to string.
            serde_json::from_str(raw).unwrap_or_else(|_| serde_json::Value::String(raw.to_string()))
        }
    }
}

/// Fill in defaults from `binds` for schema-declared parameters a call did
/// not supply. An explicit argument always wins: only properties absent from
/// `arguments` are filled in, whether the call arrived as `key=value` tokens
/// or a single JSON object literal -- both produce a JSON object here, and
/// this runs on the result either way, so a bind fills the same gaps
/// regardless of which form supplied the rest.
pub(crate) fn apply_binds(
    schema: &serde_json::Value,
    binds: &crate::bind::Binds,
    mut arguments: serde_json::Value,
) -> serde_json::Value {
    let Some(object) = arguments.as_object_mut() else {
        return arguments;
    };
    let Some(properties) = schema.get("properties").and_then(|p| p.as_object()) else {
        return arguments;
    };
    for name in properties.keys() {
        if object.contains_key(name) {
            continue;
        }
        if let Some(raw) = binds.get(name) {
            object.insert(name.clone(), coerce_arg(schema, name, raw));
        }
    }
    arguments
}

fn split_kv_arg(token: &str) -> Result<(&str, &str), String> {
    let Some((key, value)) = token.split_once('=') else {
        return Err(format!("argument {token:?} must use `key=value` syntax"));
    };
    if key.is_empty() {
        return Err(format!("argument {token:?} has an empty name"));
    }
    Ok((key, value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "count": {"type": "integer"},
                "ratio": {"type": "number"},
                "enabled": {"type": "boolean"},
                "text": {"type": "string"},
                "items": {"type": "array"},
                "metadata": {"type": "object"},
                "untyped": {}
            }
        })
    }

    #[test]
    fn schema_types_coerce_without_turning_bad_values_into_other_types() {
        let parsed = parse_kv_args(
            &schema(),
            &[
                "count=2",
                "ratio=1.5",
                "enabled=true",
                "text=null",
                "items=[1,2]",
                r#"metadata={"source":"test"}"#,
                "untyped=null",
                "bad_count=two",
            ],
        )
        .unwrap();
        assert_eq!(parsed["count"], 2);
        assert_eq!(parsed["ratio"], 1.5);
        assert_eq!(parsed["enabled"], true);
        assert_eq!(parsed["text"], "null");
        assert_eq!(parsed["items"], json!([1, 2]));
        assert_eq!(parsed["metadata"], json!({"source": "test"}));
        assert_eq!(parsed["untyped"], serde_json::Value::Null);
        assert_eq!(parsed["bad_count"], "two");
    }

    #[test]
    fn schema_declared_strings_preserve_json_looking_text() {
        for raw in ["true", "123", "null", "[1,2]", r#"{"source":"test"}"#] {
            let argument = format!("text={raw}");
            let parsed = parse_kv_args(&schema(), &[&argument]).unwrap();
            assert_eq!(parsed["text"], raw);
            assert!(parsed["text"].is_string());
        }
    }

    #[test]
    fn a_type_beside_a_reference_wins_for_arguments_and_binds() {
        let schema = json!({
            "$defs": {"Untyped": {}, "Number": {"type": "integer"}},
            "properties": {
                "local": {"$ref": "#/$defs/Untyped", "type": "string"},
                "sibling": {"$ref": "#/$defs/Number", "type": "string"},
                "inherited": {"$ref": "#/$defs/Number"},
                "nullable": {"$ref": "#/$defs/Number", "type": ["string", "null"]}
            }
        });
        let parsed = parse_kv_args(
            &schema,
            &[
                "local=123",
                "sibling=true",
                "inherited=123",
                "nullable=null",
            ],
        )
        .unwrap();
        assert_eq!(
            parsed,
            json!({
                "local": "123", "sibling": "true", "inherited": 123, "nullable": null
            })
        );
        assert_eq!(
            apply_binds(&schema, &binds(&[("local", "123")]), json!({})),
            json!({"local": "123"})
        );
    }

    #[test]
    fn a_complete_json_object_is_the_only_positional_form() {
        assert_eq!(
            parse_kv_args(&schema(), &[r#"{"count":2}"#]).unwrap(),
            json!({"count": 2})
        );
        assert!(parse_kv_args(&schema(), &["[1,2]"]).is_err());
        assert!(parse_kv_args(&schema(), &[r#"{"count":}"#]).is_err());
    }

    #[test]
    fn prompt_values_stay_strings_and_malformed_tokens_are_rejected() {
        assert_eq!(
            parse_prompt_args(&["count=2", "empty="]).unwrap(),
            HashMap::from([
                ("count".to_string(), "2".to_string()),
                ("empty".to_string(), String::new()),
            ])
        );
        for token in ["missing", "=value"] {
            assert!(parse_prompt_args(&[token]).is_err());
        }
    }

    fn binds(pairs: &[(&str, &str)]) -> crate::bind::Binds {
        let mut binds = crate::bind::Binds::default();
        for (name, value) in pairs {
            binds.set(name, value);
        }
        binds
    }

    #[test]
    fn an_explicit_argument_always_wins_over_a_bind() {
        let parsed = parse_kv_args(&schema(), &["count=2"]).unwrap();
        let filled = apply_binds(&schema(), &binds(&[("count", "9")]), parsed);
        assert_eq!(filled["count"], 2);
    }

    #[test]
    fn a_bind_fills_a_declared_property_the_call_omitted_coerced_to_its_type() {
        let parsed = parse_kv_args(&schema(), &["ratio=1.5"]).unwrap();
        let filled = apply_binds(&schema(), &binds(&[("count", "2")]), parsed);
        // A real JSON number, not the string "2": the same coercion a typed
        // `k=v` argument already gets.
        assert_eq!(filled["count"], json!(2));
        assert!(filled["count"].is_number());
        assert_eq!(filled["ratio"], 1.5);
    }

    #[test]
    fn a_string_bind_preserves_json_looking_text() {
        let parsed = parse_kv_args(&schema(), &["count=2"]).unwrap();
        let filled = apply_binds(&schema(), &binds(&[("text", "true")]), parsed);
        assert_eq!(filled["text"], "true");
        assert!(filled["text"].is_string());
    }

    #[test]
    fn a_bind_for_an_undeclared_parameter_is_never_inserted() {
        let parsed = parse_kv_args(&schema(), &["count=2"]).unwrap();
        let filled = apply_binds(&schema(), &binds(&[("nope", "1")]), parsed);
        assert!(filled.get("nope").is_none());
    }

    #[test]
    fn a_bind_fills_gaps_in_a_json_object_literal_the_same_way() {
        let parsed = parse_kv_args(&schema(), &[r#"{"count":2}"#]).unwrap();
        let filled = apply_binds(&schema(), &binds(&[("ratio", "1.5")]), parsed);
        // The literal's own key is untouched...
        assert_eq!(filled["count"], json!(2));
        // ...and the bind still fills the property the object omitted, typed
        // from the schema rather than left as text.
        assert_eq!(filled["ratio"], json!(1.5));
        assert!(filled["ratio"].is_number());
    }

    /// `analyze_crate` from the issue: one required argument, one optional.
    fn analyze_crate() -> tower_mcp::protocol::PromptDefinition {
        serde_json::from_value(json!({
            "name": "analyze_crate",
            "arguments": [
                {"name": "name", "required": true},
                {"name": "focus"}
            ]
        }))
        .unwrap()
    }

    fn prompt_args(tokens: &[&str]) -> HashMap<String, String> {
        parse_prompt_args(tokens).unwrap()
    }

    #[test]
    fn declared_prompt_arguments_pass() {
        let prompt = analyze_crate();
        assert_eq!(
            check_prompt_args(&prompt, &prompt_args(&["name=serde"])),
            Ok(())
        );
        assert_eq!(
            check_prompt_args(&prompt, &prompt_args(&["name=serde", "focus=api"])),
            Ok(())
        );
        // Present but empty is still supplied; the server judges the value.
        assert_eq!(check_prompt_args(&prompt, &prompt_args(&["name="])), Ok(()));
    }

    #[test]
    fn a_missing_required_prompt_argument_is_named() {
        let error = check_prompt_args(&analyze_crate(), &prompt_args(&["focus=api"])).unwrap_err();
        assert_eq!(
            error.message,
            r#"prompt "analyze_crate" is missing required argument "name""#
        );
        assert_eq!(error.hint, None);
    }

    #[test]
    fn every_missing_required_prompt_argument_is_named_in_declaration_order() {
        let prompt: tower_mcp::protocol::PromptDefinition = serde_json::from_value(json!({
            "name": "compare",
            "arguments": [
                {"name": "left", "required": true},
                {"name": "right", "required": true}
            ]
        }))
        .unwrap();
        let error = check_prompt_args(&prompt, &HashMap::new()).unwrap_err();
        assert_eq!(
            error.message,
            r#"prompt "compare" is missing required arguments "left", "right""#
        );
    }

    #[test]
    fn an_unknown_prompt_argument_suggests_the_closest_declared_name() {
        let error = check_prompt_args(&analyze_crate(), &prompt_args(&["nme=serde"])).unwrap_err();
        assert_eq!(
            error.message,
            r#"prompt "analyze_crate" has no argument "nme""#
        );
        assert_eq!(error.hint.as_deref(), Some("name"));
    }

    #[test]
    fn an_unknown_prompt_argument_is_reported_before_the_required_one_it_replaced() {
        // The issue's case: `crate_name` is not declared and `name` is
        // missing. Naming the unknown argument is the more useful answer, and
        // `name` is suggested because `crate_name` contains it.
        let error =
            check_prompt_args(&analyze_crate(), &prompt_args(&["crate_name=serde"])).unwrap_err();
        assert_eq!(
            error.message,
            r#"prompt "analyze_crate" has no argument "crate_name""#
        );
        assert_eq!(error.hint.as_deref(), Some("name"));
    }

    #[test]
    fn an_unknown_prompt_argument_with_nothing_close_lists_the_declared_names() {
        let error = check_prompt_args(&analyze_crate(), &prompt_args(&["zzz=1"])).unwrap_err();
        assert_eq!(
            error.message,
            r#"prompt "analyze_crate" has no argument "zzz"; it takes "name", "focus""#
        );
        assert_eq!(error.hint, None);
    }

    #[test]
    fn a_containing_or_contained_declared_name_is_suggested_case_insensitively() {
        let declared = ["crate_name", "crate_version"];
        // Too far apart for edit distance, but only one name contains it.
        assert_eq!(
            suggest_argument("version", &declared).as_deref(),
            Some("crate_version")
        );
        assert_eq!(
            suggest_argument("Name", &declared).as_deref(),
            Some("crate_name")
        );
        assert_eq!(
            suggest_argument("Crate_Name", &["name"]).as_deref(),
            Some("name")
        );
        // Both declared names contain `crate`: picking one would be a guess.
        assert_eq!(suggest_argument("crate", &declared), None);
        // An empty declared name is contained in every key; it is no answer.
        assert_eq!(suggest_argument("anything", &["", "other"]), None);
    }

    #[test]
    fn several_unknown_prompt_arguments_each_carry_their_own_suggestion() {
        let error = check_prompt_args(
            &analyze_crate(),
            &prompt_args(&["name=serde", "nme=x", "focs=y"]),
        )
        .unwrap_err();
        assert_eq!(
            error.message,
            r#"prompt "analyze_crate" has no arguments "focs" (did you mean "focus"?), "nme" (did you mean "name"?)"#
        );
        // The JSON `didYouMean` field stays a single-unknown answer.
        assert_eq!(error.hint, None);
    }

    #[test]
    fn several_unknown_prompt_arguments_list_the_declared_names_when_one_has_no_suggestion() {
        let error =
            check_prompt_args(&analyze_crate(), &prompt_args(&["nme=x", "zzz=y"])).unwrap_err();
        assert_eq!(
            error.message,
            r#"prompt "analyze_crate" has no arguments "nme" (did you mean "name"?), "zzz"; it takes "name", "focus""#
        );
        assert_eq!(error.hint, None);
    }

    #[test]
    fn a_prompt_that_declares_no_arguments_passes_everything_through() {
        // tower-mcp reads an omitted `arguments` and `[]` alike, and a server
        // may accept arguments it never advertised.
        for definition in [
            json!({"name": "status"}),
            json!({"name": "status", "arguments": []}),
        ] {
            let prompt: tower_mcp::protocol::PromptDefinition =
                serde_json::from_value(definition).unwrap();
            assert_eq!(check_prompt_args(&prompt, &HashMap::new()), Ok(()));
            assert_eq!(
                check_prompt_args(&prompt, &prompt_args(&["verbose=true"])),
                Ok(())
            );
        }
    }
}
