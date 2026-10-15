//! MCP tool input validation and result framing.

use relayburn_sdk::Enrichment;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Map, Value};

pub(super) fn object_input<'a>(
    raw: &'a Value,
    tool: &str,
    allowed: &[&str],
) -> Result<&'a Map<String, Value>, String> {
    let Some(input) = raw.as_object() else {
        return Err(format!("{tool}: input must be an object"));
    };
    if let Some(key) = input.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(format!("{tool}: unknown property {key}"));
    }
    Ok(input)
}

pub(super) fn optional_string(
    input: &Map<String, Value>,
    key: &str,
    tool: &str,
) -> Result<Option<String>, String> {
    let Some(value) = input.get(key) else {
        return Ok(None);
    };
    value
        .as_str()
        .map(|value| Some(value.to_string()))
        .ok_or_else(|| format!("{tool}: {key} must be a string"))
}

pub(super) fn optional_boolean(
    input: &Map<String, Value>,
    key: &str,
    tool: &str,
) -> Result<Option<bool>, String> {
    let Some(value) = input.get(key) else {
        return Ok(None);
    };
    value
        .as_bool()
        .map(Some)
        .ok_or_else(|| format!("{tool}: {key} must be a boolean"))
}

pub(super) fn optional_u32(
    input: &Map<String, Value>,
    key: &str,
    tool: &str,
) -> Result<Option<u32>, String> {
    let Some(value) = input.get(key) else {
        return Ok(None);
    };
    let value = value
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .or_else(|| {
            value.as_f64().and_then(|value| {
                (value.is_finite()
                    && value.fract() == 0.0
                    && value >= 0.0
                    && value <= f64::from(u32::MAX))
                .then_some(value as u32)
            })
        });
    let Some(value) = value else {
        return Err(format!("{tool}: {key} must be a 32-bit unsigned integer"));
    };
    Ok(Some(value))
}

pub(super) fn optional_string_array(
    input: &Map<String, Value>,
    key: &str,
    tool: &str,
) -> Result<Option<Vec<String>>, String> {
    let Some(value) = input.get(key) else {
        return Ok(None);
    };
    let Some(items) = value.as_array() else {
        return Err(format!("{tool}: {key} must be an array of strings"));
    };
    let values: Option<Vec<String>> = items
        .iter()
        .map(|item| item.as_str().map(str::to_string))
        .collect();
    values
        .map(Some)
        .ok_or_else(|| format!("{tool}: {key} must be an array of strings"))
}

pub(super) fn required_string_array(
    input: &Map<String, Value>,
    key: &str,
    tool: &str,
    minimum: usize,
) -> Result<Vec<String>, String> {
    let value = optional_string_array(input, key, tool)?;
    match value {
        Some(items) if items.len() >= minimum => Ok(items),
        _ => Err(format!(
            "{tool}: {key} must contain at least {minimum} strings"
        )),
    }
}

pub(super) fn optional_string_record(
    input: &Map<String, Value>,
    key: &str,
    tool: &str,
) -> Result<Option<Enrichment>, String> {
    let Some(value) = input.get(key) else {
        return Ok(None);
    };
    let Some(record) = value.as_object() else {
        return Err(format!(
            "{tool}: {key} must be an object with string values"
        ));
    };
    let mut result = Enrichment::new();
    for (record_key, value) in record {
        let Some(value) = value.as_str() else {
            return Err(format!(
                "{tool}: {key} must be an object with string values"
            ));
        };
        result.insert(record_key.clone(), value.to_string());
    }
    Ok(Some(result))
}

pub(super) fn optional_enum<T>(
    input: &Map<String, Value>,
    key: &str,
    tool: &str,
    allowed: &[&str],
) -> Result<Option<T>, String>
where
    T: DeserializeOwned,
{
    let Some(value) = input.get(key) else {
        return Ok(None);
    };
    let Some(raw) = value.as_str() else {
        return Err(format!("{tool}: {key} must be a string"));
    };
    if !allowed.contains(&raw) {
        return Err(format!(
            "{tool}: {key} must be one of {}",
            allowed.join(", ")
        ));
    }
    serde_json::from_value(value.clone())
        .map(Some)
        .map_err(|err| format!("{tool}: invalid {key}: {err}"))
}

pub(super) fn tool_error(err: impl std::fmt::Display) -> Value {
    json!({
        "content": [{ "type": "text", "text": err.to_string() }],
        "isError": true,
    })
}

pub(super) fn tool_output(payload: &impl Serialize) -> Value {
    match serde_json::to_value(payload) {
        Ok(value) => match serde_json::to_string(&value) {
            Ok(text) => json!({
                "content": [{ "type": "text", "text": text }],
                "structuredContent": value,
            }),
            Err(err) => tool_error(format!("failed to encode tool result: {err}")),
        },
        Err(err) => tool_error(format!("failed to encode tool result: {err}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn booleans_are_read_as_given_and_rejected_otherwise() {
        let read = |value: Value| optional_boolean(&input(value), "includeDiff", "trim");
        assert_eq!(read(json!({})), Ok(None));
        assert_eq!(read(json!({"includeDiff": true})), Ok(Some(true)));
        assert_eq!(read(json!({"includeDiff": false})), Ok(Some(false)));
        assert_eq!(
            read(json!({"includeDiff": "yes"})),
            Err("trim: includeDiff must be a boolean".to_string())
        );
    }

    #[test]
    fn a_required_array_needs_its_minimum_length() {
        let read = |value: Value| required_string_array(&input(value), "models", "compare", 2);
        assert_eq!(
            read(json!({"models": ["a", "b"]})),
            Ok(vec!["a".to_string(), "b".to_string()])
        );
        let short = Err("compare: models must contain at least 2 strings".to_string());
        assert_eq!(read(json!({"models": ["a"]})), short);
        assert_eq!(read(json!({})), short);
    }
}
