//! The domain: a conversation reconstructed from the wire, as a list of turns.

pub(crate) mod turn;

use crate::{Duration, HashSet, Instant, is_subagent_tool, str_field};
use crate::chat::turn::{ToolCall, Turn};
use serde_json::Value;

/// How long a chat that already finished may stay open after its last chunk:
/// the silent gap between two turns of an interactive session, when nothing is
/// on the wire yet but the harness is still in use.
pub(crate) const LIVE_GRACE: Duration = Duration::from_secs(30);

pub(crate) struct Chat {
    pub(crate) name: Option<String>,
    pub(crate) model: String,
    pub(crate) backend: Option<String>,
    pub(crate) flow: String,
    /// Explicit thread identity declared by the client (session header or
    /// body field), when it sent one. Authoritative over content heuristics.
    pub(crate) thread: Option<String>,
    /// User-Agent of the client that sent the first observed request.
    /// Identifies the caller (e.g. `pi/1.4.0`) without decoding the body.
    pub(crate) client: Option<String>,
    /// First-turn id of the chat this one was spawned from, when it is a
    /// subagent generation. Such chats are grouped under their parent tab
    /// instead of being merged into its message history.
    pub(crate) parent: Option<String>,
    /// Last time a chunk arrived, used to bound subagent auto-grouping.
    pub(crate) last_activity: Instant,
    pub(crate) turns: Vec<Turn>,
    pub(crate) scroll: u16,
    pub(crate) follow: bool,
}

impl Chat {
    pub(crate) fn new(
        id: String,
        request: Option<Value>,
        model: String,
        flow: String,
        thread: Option<String>,
        client: Option<String>,
    ) -> Self {
        Self {
            name: None,
            model,
            backend: None,
            flow,
            thread,
            client,
            parent: None,
            last_activity: Instant::now(),
            turns: vec![Turn::new(id, request)],
            scroll: 0,
            follow: true,
        }
    }
    /// Scrolling back stops following the stream; scrolling forward does not
    /// resume it (End does).
    pub(crate) fn scroll_up(&mut self, rows: u16) {
        self.follow = false;
        self.scroll = self.scroll.saturating_sub(rows);
    }
    pub(crate) fn scroll_down(&mut self, rows: u16) {
        self.scroll = self.scroll.saturating_add(rows);
    }
    pub(crate) fn id(&self) -> &str {
        &self.turns[0].id
    }
    pub(crate) fn last(&self) -> &Turn {
        self.turns
            .last()
            .expect("a chat always has at least one turn")
    }
    pub(crate) fn has_id(&self, id: &str) -> bool {
        self.turns.iter().any(|t| t.id == id)
    }
    pub(crate) fn finish(&self) -> Option<&str> {
        self.last().finish.as_deref()
    }
    /// True while the conversation is still going: the last turn is generating,
    /// or ended asking for tool calls (the client is executing them, or a
    /// subagent is), or produced output inside the live-grace window.
    pub(crate) fn is_running(&self) -> bool {
        match self.finish() {
            // No finish reason: still streaming, or the finish chunk was lost.
            None => true,
            Some("tool_calls") => true,
            Some(_) => self.last_activity.elapsed() < LIVE_GRACE,
        }
    }
    pub(crate) fn chunks(&self) -> usize {
        self.turns.iter().map(|t| t.chunks).sum()
    }
    pub(crate) fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.turns.iter().flat_map(|t| t.tool_calls.iter())
    }
    pub(crate) fn usage(&self) -> Option<&Value> {
        self.turns.iter().rev().find_map(|t| t.usage.as_ref())
    }
    pub(crate) fn timings(&self) -> Option<&Value> {
        self.turns.iter().rev().find_map(|t| t.timings.as_ref())
    }
    /// Tool calls in progress that may have spawned subagent generations. The
    /// turn must have finished asking for tools, and no later request may have
    /// delivered a result for the call yet.
    pub(crate) fn open_subagent_calls(&self) -> Vec<&ToolCall> {
        // `None` means the turn is still streaming: a harness may execute a
        // completed tool call before the finish chunk reaches the wire.
        if !matches!(self.finish(), Some("tool_calls") | None) {
            return Vec::new();
        }
        let resolved: HashSet<&str> = self
            .last()
            .messages()
            .into_iter()
            .flatten()
            .filter(|m| str_field(m, "role") == Some("tool"))
            .filter_map(|m| str_field(m, "tool_call_id"))
            .collect();
        self.last()
            .tool_calls
            .iter()
            .filter(|tc| is_subagent_tool(&tc.name) && !resolved.contains(tc.id.as_str()))
            .collect()
    }
}
