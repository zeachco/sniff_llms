//! Small accessors over `serde_json::Value` for the OpenAI-style bodies on the wire.

use serde_json::Value;

pub(crate) fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

pub(crate) fn str_or<'a>(v: &'a Value, key: &str, default: &'a str) -> &'a str {
    str_field(v, key).unwrap_or(default)
}

pub(crate) fn request_messages<'a>(v: &'a Value) -> Option<&'a [Value]> {
    v.get("messages").and_then(Value::as_array).map(Vec::as_slice)
}

/// The `tool_calls` array of a message or a streamed delta.
pub(crate) fn tool_calls_of(v: &Value) -> Option<&[Value]> {
    v.get("tool_calls").and_then(Value::as_array).map(Vec::as_slice)
}

pub(crate) fn text_of(v: &Value, pretty: bool) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => {
            if pretty {
                serde_json::to_string_pretty(other).unwrap_or_default()
            } else {
                other.to_string()
            }
        }
    }
}

pub(crate) fn number_at<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter()
        .find_map(|key| value.get(*key))
        .filter(|v| v.is_number())
}
