//! String shaping for the UI: eliding, truncating, compacting and durations.

use serde_json::Value;
use std::time::Duration;

pub(crate) fn safe_name(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || ".-_".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect()
}

pub(crate) fn middle_elide(s: &str, max_chars: usize) -> String {
    let chars = s.chars().collect::<Vec<_>>();
    // why `< 5`: below that the "..." leaves no room for a character on each
    // side. No caller asks for so little today; the guard keeps the
    // subtraction below from underflowing if one ever does.
    if chars.len() <= max_chars || max_chars < 5 {
        return s.into();
    }
    let kept = max_chars - 3;
    let left = kept.div_ceil(2);
    let right = kept / 2;
    chars[..left]
        .iter()
        .chain(['.', '.', '.'].iter())
        .chain(chars[chars.len() - right..].iter())
        .collect()
}

/// Re-serialize tool-call arguments as compact single-line JSON so the log does
/// not fragment across many lines. Falls back to the raw text (with internal
/// newlines collapsed to spaces) when it is not valid JSON.
pub(crate) fn compact_arguments(raw: &str) -> String {
    match serde_json::from_str::<Value>(raw) {
        Ok(parsed) => serde_json::to_string(&parsed).unwrap_or_else(|_| raw.to_string()),
        Err(_) => raw.split_whitespace().collect::<Vec<_>>().join(" "),
    }
}

pub(crate) fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if d.as_millis() < 1000 {
        format!("{}ms", d.as_millis())
    } else if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

pub(crate) fn first_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

pub(crate) fn last_chars(s: &str, n: usize) -> String {
    s.chars()
        .rev()
        .take(n)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}
