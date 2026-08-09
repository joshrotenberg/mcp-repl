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
        assert_eq!(parsed["items"], json!([1, 2]));
        assert_eq!(parsed["metadata"], json!({"source": "test"}));
        assert_eq!(parsed["untyped"], serde_json::Value::Null);
        assert_eq!(parsed["bad_count"], "two");
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
}
