//! The plain-text rendering of a chat: the stats block and the per-turn
//! sections shared by the dump file and the body pane.

use crate::chat::Chat;
use crate::chat::turn::Turn;
use crate::json::{number_at, request_messages, str_or, text_of, tool_calls_of};
use crate::text::{compact_arguments, middle_elide};
use serde_json::Value;

/// A stat that may be missing from the server's report.
fn shown(number: Option<&Value>) -> String {
    number.map_or("?".into(), Value::to_string)
}

pub(crate) fn stats_text(c: &Chat) -> String {
    let mut stats = vec![format!("ID: {}", c.id())];
    if let Some(name) = c.name.as_ref() {
        stats.push(format!("Name: {name}"));
    }
    stats.push(format!("Model: {}", c.model));
    if let Some(client) = c.client.as_deref() {
        stats.push(format!("Client: {client}"));
    }
    if let Some(backend) = c.backend.as_deref() {
        stats.push(format!("Backend: {backend}"));
    }
    stats.extend([
        format!("Finish: {}", c.finish().unwrap_or("streaming")),
        format!("Chunks: {}", c.chunks()),
        format!("Flow: {}", c.flow),
    ]);
    if let Some(thread) = c.thread.as_deref() {
        stats.push(format!("Thread: {thread}"));
    }
    if let Some(parent) = c.parent.as_deref() {
        stats.push(format!("Parent: {}", middle_elide(parent, 32)));
    }
    if c.turns.len() > 1 {
        stats.push(format!("Turns: {}", c.turns.len()));
    }
    let tool_count = c
        .tool_calls()
        .filter(|tc| !tc.is_empty())
        .count();
    if tool_count > 0 {
        stats.push(format!("Tool calls: {tool_count}"));
    }
    if let Some(usage) = c.usage() {
        let prompt = number_at(usage, &["prompt_tokens"]);
        let completion = number_at(usage, &["completion_tokens"]);
        let total = number_at(usage, &["total_tokens"]);
        if prompt.is_some() || completion.is_some() || total.is_some() {
            stats.push(format!(
                "Tokens: prompt {} | completion {} | total {}",
                shown(prompt),
                shown(completion),
                shown(total),
            ));
        }
    }
    if let Some(timings) = c.timings() {
        let predicted = number_at(timings, &["predicted_per_second", "tokens_per_second"]);
        let prompt = number_at(timings, &["prompt_per_second"]);
        if predicted.is_some() || prompt.is_some() {
            stats.push(format!(
                "Speed: generation {} tok/s | prompt {} tok/s",
                shown(predicted),
                shown(prompt),
            ));
        }
    }
    stats.join("\n")
}

fn messages_text(messages: &[Value], skip: usize) -> String {
    let mut out = String::new();
    for m in messages.iter().skip(skip) {
        let role = str_or(m, "role", "unknown");
        let content = m.get("content").map(|v| text_of(v, true)).unwrap_or_default();
        out.push_str(&format!("[{role}]\n{content}\n"));
        if let Some(calls) = tool_calls_of(m) {
            for call in calls {
                let name = call
                    .pointer("/function/name")
                    .and_then(Value::as_str)
                    .unwrap_or("function");
                let args = call
                    .pointer("/function/arguments")
                    .and_then(Value::as_str)
                    .map(compact_arguments)
                    .unwrap_or_default();
                out.push_str(&format!("  → {name}({args})\n"));
            }
        }
        out.push('\n');
    }
    out
}

fn request_text(request: Option<&Value>) -> String {
    let Some(r) = request else {
        return "(request began before capture)".into();
    };
    if let Some(messages) = request_messages(r) {
        messages_text(messages, 0)
    } else {
        serde_json::to_string_pretty(r).unwrap_or_default()
    }
}

/// Only the messages a continuation turn added, so a long conversation is not
/// printed again for every tool round trip.
fn request_delta_text(previous: Option<&Value>, current: Option<&Value>) -> String {
    let (Some(previous), Some(current)) = (previous, current) else {
        return request_text(current);
    };
    let (Some(prev), Some(new)) = (request_messages(previous), request_messages(current))
    else {
        return request_text(Some(current));
    };
    let common = new
        .iter()
        .zip(prev)
        .take_while(|(new, prev)| new == prev)
        .count();
    if common == 0 {
        return request_text(Some(current));
    }
    messages_text(new, common)
}

fn turn_request_text(turns: &[Turn], index: usize) -> String {
    let turn = &turns[index];
    match index.checked_sub(1) {
        Some(previous) => {
            request_delta_text(turns[previous].request.as_ref(), turn.request.as_ref())
        }
        None => request_text(turn.request.as_ref()),
    }
}

pub(crate) fn tool_calls_text(turn: &Turn) -> String {
    let mut out = String::new();
    for tc in &turn.tool_calls {
        if tc.is_empty() {
            continue;
        }
        let name: &str = if tc.name.is_empty() {
            "function"
        } else {
            &tc.name
        };
        out.push_str(&format!("[{}] {name}\n", tc.id));
        let args = compact_arguments(&tc.arguments);
        if !args.is_empty() {
            out.push_str(&format!("    {args}\n"));
        }
    }
    out.trim_end().to_string()
}

#[derive(Clone, Copy)]
pub(crate) enum Section {
    Turn,
    Request,
    Reasoning,
    Answer,
    ToolCalls,
}

/// The sections of one turn, in display order. The single source of the dump
/// and body layout: render_chat, render_body and the anchor math in
/// segment_anchor_rows all consume this, so the layout can only change here.
pub(crate) fn turn_sections(c: &Chat, index: usize) -> Vec<(Section, String, String)> {
    let turn = &c.turns[index];
    let mut sections = Vec::new();
    if c.turns.len() > 1 {
        let header = format!("=== TURN {} ===", index + 1);
        sections.push((Section::Turn, header, String::new()));
    }
    let request = turn_request_text(&c.turns, index);
    sections.push((Section::Request, "=== REQUEST ===".into(), request));
    let reasoning = turn.reasoning.clone();
    sections.push((Section::Reasoning, "=== REASONING ===".into(), reasoning));
    let answer = turn.answer.clone();
    sections.push((Section::Answer, "=== ANSWER ===".into(), answer));
    if !turn.tool_calls.is_empty() {
        let calls = tool_calls_text(turn);
        sections.push((Section::ToolCalls, "=== TOOL CALLS ===".into(), calls));
    }
    sections
}

pub(crate) fn render_chat(c: &Chat) -> String {
    let mut out = format!("{}\n\n", stats_text(c));
    for index in 0..c.turns.len() {
        for (_, header, text) in turn_sections(c, index) {
            // Same line layout as body_section: header, text lines, blank.
            out.push_str(&header);
            out.push('\n');
            for line in text.lines() {
                out.push_str(line);
                out.push('\n');
            }
            out.push('\n');
        }
    }
    out
}
