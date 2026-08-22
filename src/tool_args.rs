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

/// Coerce a `key=value` string according to the tool's input schema.
fn coerce_arg(schema: &serde_json::Value, key: &str, raw: &str) -> serde_json::Value {
    let ty = schema
        .get("properties")
        .and_then(|properties| properties.get(key))
        .and_then(|property| property.get("type"))
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
}
