//! Recognising subagent traffic: the tools that spawn one, the requests one
//! sends, and the brief a parent passes down.

use crate::json::{request_messages, text_of};
use serde_json::Value;

/// Tool names a harness uses to spawn a child generation. Cindy/pi's subagent
/// extension uses `subagent`; the others cover common conventions.
pub(crate) fn is_subagent_tool(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "subagent" | "spawn_agent" | "task" | "delegate"
    )
}

/// A child generation carries one of the subagent profile prompts that the
/// spawning extension appends. Normal conversations must not be grouped just
/// because some parent is waiting on a tool.
pub(crate) fn looks_like_subagent_request(request: &Value) -> bool {
    // Match the opening line of each pi-subagents agent profile, which the
    // harness puts in the child's system prompt.
    let text = request_plain_text(request).to_ascii_lowercase();
    [
        "you are a scout.",
        "you are the executor.",
        "you are the planner.",
        "you are a senior reviewer.",
        "you are the typing tier",
        "you are a fast, literal coder.",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}

/// Shorter strings in tool-call arguments are too generic to identify a brief.
pub(crate) const MIN_BRIEF_CHARS: usize = 40;

/// Bytes of request text scanned for a brief; huge histories stop here.
const REQUEST_TEXT_SCAN_CAP: usize = 256 * 1024;

/// Unescaped text of a request's messages, for substring matching against the
/// brief a parent passed to its subagent tool.
pub(crate) fn request_plain_text(request: &Value) -> String {
    let Some(messages) = request_messages(request) else {
        return String::new();
    };
    let mut out = String::new();
    for message in messages {
        if let Some(content) = message.get("content") {
            out.push_str(&text_of(content, false));
        }
        out.push('\n');
        if out.len() > REQUEST_TEXT_SCAN_CAP {
            break;
        }
    }
    out
}

/// Long string values embedded in a tool call's JSON arguments: task briefs,
/// prompts and instructions. These are the phrases a child generation echoes
/// back in its own request.
pub(crate) fn briefs(arguments: &str) -> Vec<String> {
    fn walk(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::String(s) if s.chars().count() >= MIN_BRIEF_CHARS => out.push(s.clone()),
            Value::Array(items) => items.iter().for_each(|item| walk(item, out)),
            Value::Object(map) => map.values().for_each(|item| walk(item, out)),
            _ => {}
        }
    }
    let Ok(parsed) = serde_json::from_str::<Value>(arguments) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    walk(&parsed, &mut out);
    out
}
