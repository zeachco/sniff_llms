//! Naming a chat with a local model.

use crate::capture::resolve_binary;
use crate::chat::Chat;
use crate::chat::turn::Turn;
use crate::json::str_field;
use crate::text::{first_chars, last_chars};
use std::io::Write;
use std::process::{Command, Stdio};

/// A chat id and the name the namer produced for it, or why it failed.
pub(crate) type NameResult = (String, Result<String, String>);

/// How much of the conversation's two ends the namer gets to read.
const NAME_HEAD_CHARS: usize = 100;

const NAME_TAIL_CHARS: usize = 1000;

pub(crate) fn name_prompt(c: &Chat) -> String {
    let first_user = c
        .turns
        .first()
        .and_then(Turn::messages)
        .and_then(|m| m.iter().find(|m| str_field(m, "role") == Some("user")))
        .and_then(|m| str_field(m, "content"))
        .map(|s| first_chars(s, NAME_HEAD_CHARS))
        .unwrap_or_default();
    let tail = last_chars(&c.last().answer, NAME_TAIL_CHARS);
    format!(
        "Name this chat conversation in 2-6 words. Reply with ONLY the name, no quotes, no trailing punctuation.\n\nFirst user message (first {NAME_HEAD_CHARS} chars):\n{first_user}\n\nEnd of conversation (last {NAME_TAIL_CHARS} chars):\n{tail}"
    )
}

pub(crate) fn run_namer(model: &str, prompt: &str) -> Result<String, String> {
    let namer = resolve_binary("ollama").ok_or_else(|| "ollama not found in PATH".to_string())?;
    let mut child = Command::new(namer)
        .args(["run", "--nowordwrap", model])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn ollama: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(prompt.as_bytes())
            .map_err(|e| e.to_string())?;
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_string())
        .ok_or_else(|| "empty model output".into())
}
