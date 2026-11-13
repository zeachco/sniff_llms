//! Reading what ngrep hands over: packet headers, HTTP framing, direction,
//! JSON objects split across packets, and the thread identity a client declares.

use crate::json::str_field;
use crate::text::last_chars;
use serde_json::Value;

pub(crate) struct Packet {
    pub(crate) header: String,
    pub(crate) payload: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dir {
    Request,
    Response,
}

pub(crate) fn parse_header(s: &str) -> Option<(&str, &str)> {
    let mut p = s.split_whitespace();
    if p.next()? != "T" {
        return None;
    }
    let src = p.next()?;
    if p.next()? != "->" {
        return None;
    }
    Some((src, p.next()?))
}

pub(crate) fn is_request(v: &Value) -> bool {
    v.get("messages").is_some_and(Value::is_array) || v.get("prompt").is_some_and(Value::is_string)
}

pub(crate) fn is_llm_response(v: &Value) -> bool {
    v.get("id").is_some_and(|v| v.is_string())
        && (v.get("choices").is_some_and(Value::is_array)
            || v.get("completion").is_some()
            || v.get("prompt").is_some_and(Value::is_array))
}

/// Guess the serving backend from response-only signals. `None` means
/// "plain OpenAI-compatible, no distinguishing marker".
pub(crate) fn detect_backend(v: &Value) -> Option<&'static str> {
    if v.get("timings").is_some() {
        return Some("llama.cpp");
    }
    if v.get("prompt_eval_count").is_some()
        || v.get("eval_count").is_some()
        || v.get("done_reason").is_some()
    {
        return Some("ollama");
    }
    if v.get("provider_name").is_some() {
        return Some("openrouter");
    }
    if let Some(id) = str_field(v, "id") {
        if id.starts_with("gen-") {
            return Some("openrouter");
        }
        if id.starts_with("msg_") {
            return Some("anthropic");
        }
    }
    None
}

/// Cheap pre-filter so all-port capture does not spin up flows for non-LLM HTTP.
pub(crate) fn looks_like_llm(payload: &str) -> bool {
    let p = payload.to_ascii_lowercase();
    p.contains("/completions")
        || p.contains("\"choices\"")
        || p.contains("\"messages\"")
        || p.contains("\"completion\"")
        || p.contains("\"tool_calls\"")
}

/// Headers a client uses to declare a stable conversation identity. The value
/// is the same on every request of one thread, unlike response ids.
const THREAD_HEADERS: &[&str] = &[
    "x-session-id",
    "x-session-affinity",
    "session-id",
    "session_id",
    "x-conversation-id",
    "conversation-id",
    "conversation_id",
    "x-thread-id",
    "thread-id",
    "thread_id",
    "x-chat-id",
    "chat-id",
    "chat_id",
];

/// Body fields some clients use for the same purpose.
const THREAD_BODY_FIELDS: &[&str] = &[
    "prompt_cache_key",
    "conversation_id",
    "session_id",
    "thread_id",
];

/// Shorter values are too generic to identify a thread (`1`, `default`) and
/// are ignored, leaving the content heuristics in charge.
const MIN_THREAD_KEY_LEN: usize = 8;

pub(crate) fn thread_key_from_headers(payload: &str) -> Option<String> {
    for line in payload.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            break; // end of the header block
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        if !THREAD_HEADERS.contains(&name.as_str()) {
            continue;
        }
        let value = value.trim();
        if value.len() >= MIN_THREAD_KEY_LEN {
            // Source-agnostic: a client that switches between a header and a
            // body field mid-thread keeps the same key.
            return Some(format!("thread:{value}"));
        }
    }
    None
}

/// The request's User-Agent, when the header was captured. Identifies the
/// calling client (pi version, other agents) without decoding the body.
pub(crate) fn user_agent_from_headers(payload: &str) -> Option<String> {
    for line in payload.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            break; // end of the header block
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("user-agent") {
            let value = value.trim();
            return (!value.is_empty()).then(|| value.to_owned());
        }
    }
    None
}

pub(crate) fn thread_key_from_body(body: &Value) -> Option<String> {
    fn usable(value: &str) -> Option<String> {
        (value.len() >= MIN_THREAD_KEY_LEN).then(|| format!("thread:{value}"))
    }
    for field in THREAD_BODY_FIELDS {
        if let Some(key) = str_field(body, field).and_then(usable) {
            return Some(key);
        }
    }
    let metadata = body.get("metadata")?;
    THREAD_BODY_FIELDS
        .iter()
        .find_map(|field| str_field(metadata, field).and_then(usable))
}

/// First balanced JSON object in `s`, scanning past SSE `data:` prefixes.
fn first_json(s: &str) -> Option<Value> {
    let start = s.find('{')?;
    let bytes = s.as_bytes();
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    for i in start..bytes.len() {
        let b = bytes[i];
        if quoted {
            if escaped {
                escaped = false
            } else if b == b'\\' {
                escaped = true
            } else if b == b'"' {
                quoted = false
            }
        } else {
            match b {
                b'"' => quoted = true,
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return serde_json::from_str(&s[start..=i]).ok();
                    }
                }
                _ => {}
            }
        }
    }
    None
}

/// Direction from HTTP framing alone: a status line means the packet comes
/// from the server, a request line means it comes from the client. `None`
/// when the first non-empty line is body data.
pub(crate) fn framed_direction(payload: &str) -> Option<Dir> {
    for line in payload.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if t.starts_with("HTTP/") {
            return Some(Dir::Response); // status line
        }
        let method = t.split_whitespace().next().unwrap_or("");
        if ["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS"].contains(&method) {
            return Some(Dir::Request); // request line
        }
        return None; // first non-empty line is data, not a header
    }
    None
}

/// Decide the direction of a packet from its HTTP framing or JSON shape.
pub(crate) fn classify(payload: &str) -> Option<Dir> {
    if let Some(d) = framed_direction(payload) {
        return Some(d);
    }
    let v = first_json(payload)?;
    if v.get("choices").is_some() || v.get("completion").is_some() {
        Some(Dir::Response)
    } else if v.get("messages").is_some() || v.get("prompt").is_some() {
        Some(Dir::Request)
    } else {
        None
    }
}

// Why hand-rolled: streamed SSE chunks split JSON objects across packet
// boundaries, so serde_json's StreamDeserializer cannot be used incrementally
// here — objects are only complete once brace depth returns to zero inside
// this growing buffer. The tail cap keeps a long non-JSON prefix from growing
// the buffer without bound.
const NON_JSON_TAIL_CHARS: usize = 4096;

pub(crate) fn extract_objects(buffer: &mut String) -> Vec<Value> {
    let mut out = vec![];
    let mut start = None;
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    let mut consumed = 0;
    for (i, b) in buffer.as_bytes().iter().copied().enumerate() {
        if start.is_none() {
            if b == b'{' {
                start = Some(i);
                depth = 1;
            }
            continue;
        }
        if quoted {
            if escaped {
                escaped = false
            } else if b == b'\\' {
                escaped = true
            } else if b == b'"' {
                quoted = false
            };
            continue;
        }
        match b {
            b'"' => quoted = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    if let Ok(v) = serde_json::from_str(&buffer[start.unwrap()..=i]) {
                        out.push(v)
                    }
                    consumed = i + 1;
                    start = None;
                }
            }
            _ => {}
        }
    }
    *buffer = if let Some(i) = start {
        buffer[i..].to_owned()
    } else {
        last_chars(&buffer[consumed..], NON_JSON_TAIL_CHARS)
    };
    out
}
