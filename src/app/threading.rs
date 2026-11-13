//! Deciding which chat a new response continues, and which chat spawned it
//! as a subagent.

use crate::app::App;
use crate::json::{request_messages, str_field, tool_calls_of};
use crate::subagent::{MIN_BRIEF_CHARS, briefs, looks_like_subagent_request, request_plain_text};
use crate::text::first_chars;
use serde_json::Value;
use std::collections::HashSet;
use std::time::Duration;

impl App {
    /// Find the chat a new response belongs to. A declared thread id is
    /// authoritative. Otherwise the strongest signal is the tool-call id: the
    /// new request echoes the assistant `tool_calls` that the previous turn
    /// emitted, which works even when the previous request was never captured
    /// (capture started late) or when the client rewrites its history between
    /// calls (dynamic system prompts). When no id is available, fall back to a
    /// message-for-message prefix match followed by tool output.
    pub(crate) fn continuation(
        &self,
        flow_key: &str,
        request: Option<&Value>,
        thread: Option<&str>,
    ) -> Option<usize> {
        let Some(request) = request else {
            // The request was not captured. If this response arrives on the
            // very connection whose last response ended with tool calls, it is
            // the tool round trip: HTTP/1.1 keep-alive serialises requests, so
            // there is no competing session on that connection.
            return self.chats.iter().rposition(|chat| {
                chat.flow == flow_key
                    && (chat.last().finish.as_deref() == Some("tool_calls")
                        || !chat.last().tool_calls.is_empty())
            });
        };
        let new_msgs = request_messages(request)?;
        let request_ids: HashSet<&str> = new_msgs
            .iter()
            .filter_map(tool_calls_of)
            .flatten()
            .filter_map(|call| str_field(call, "id"))
            .collect();
        // A declared id narrows the candidates but does not have to identify a
        // single chat: a parent and its grouped subagent can share the root
        // session id sent by the harness. Content still decides between them.
        let candidates: Vec<usize> = match thread {
            Some(thread) => self
                .chats
                .iter()
                .enumerate()
                .filter(|(_, chat)| chat.thread.as_deref() == Some(thread))
                .map(|(index, _)| index)
                .collect(),
            None => (0..self.chats.len()).collect(),
        };
        if thread.is_some() && candidates.is_empty() {
            // A brand-new conversation declaring its own id.
            return None;
        }
        let mut best: Option<((usize, usize), usize)> = None;
        for index in candidates.iter().copied() {
            let turn = self.chats[index].last();
            let tool_ids: HashSet<&str> = turn
                .tool_calls
                .iter()
                .map(|tc| tc.id.as_str())
                .filter(|id| !id.is_empty())
                .collect();
            let id_matches = tool_ids.intersection(&request_ids).count();
            let prefix_len = turn
                .messages()
                .filter(|prev| {
                    !prev.is_empty()
                        && new_msgs.len() > prev.len()
                        && new_msgs
                            .iter()
                            .zip(prev.iter())
                            .all(|(new, prev)| new == prev)
                })
                .map_or(0, |prev| prev.len());
            if id_matches == 0 && prefix_len == 0 {
                continue;
            }
            // A prefix-only match could be a different conversation that shares
            // a common opening: require tool output right after the prefix.
            if id_matches == 0
                && !new_msgs[prefix_len..]
                    .iter()
                    .any(|m| str_field(m, "role") == Some("tool") || m.get("tool_calls").is_some())
            {
                continue;
            }
            let score = (id_matches, prefix_len);
            if best.is_none_or(|(best_score, _)| score >= best_score) {
                best = Some((score, index));
            }
        }
        if let Some((_, index)) = best {
            return Some(index);
        }
        // The id alone is enough when no history was captured: pick the chat
        // that was active most recently among those sharing it.
        thread.and_then(|_| {
            candidates
                .into_iter()
                .max_by_key(|&index| self.chats[index].last_activity)
        })
    }
}

/// Weight of a brief echo in the subagent-parent score. A brief match is a
/// much stronger signal than the profile prompt or the timing-only fallback,
/// so it dominates: any brief match outranks any combination without one.
const BRIEF_MATCH_WEIGHT: usize = 10;

/// A parent that has been silent this long is no longer waiting on a subagent.
const SUBAGENT_WINDOW: Duration = Duration::from_secs(300);

/// How much of a brief's opening is searched for in the child request.
const BRIEF_NEEDLE_CHARS: usize = 80;

impl App {
    /// Find the chat that most likely spawned this generation as a subagent.
    /// A parent qualifies while it waits on an unresolved subagent tool call.
    /// The brief it passed is matched against the child request when captured;
    /// otherwise the most recently active parent wins.
    pub(crate) fn subagent_parent(&self, request: Option<&Value>) -> Option<usize> {
        // A normal conversation must never be grouped just because some other
        // chat is waiting on a tool. A captured request qualifies only when it
        // carries a subagent profile prompt or echoes the brief the parent
        // passed to the tool. When the request was missed, the timing and open
        // tool-call window is all that remains.
        let plain = request.map(request_plain_text).unwrap_or_default();
        let profile = request.is_some_and(looks_like_subagent_request);
        let mut best: Option<(usize, usize)> = None;
        for (index, chat) in self.chats.iter().enumerate() {
            let calls = chat.open_subagent_calls();
            if calls.is_empty() {
                continue;
            }
            if chat.last_activity.elapsed() > SUBAGENT_WINDOW {
                continue;
            }
            let brief_match = calls.iter().any(|call| {
                briefs(&call.arguments).iter().any(|brief| {
                    let needle = first_chars(brief, BRIEF_NEEDLE_CHARS);
                    needle.chars().count() >= MIN_BRIEF_CHARS && plain.contains(&needle)
                })
            });
            if request.is_some() && !profile && !brief_match {
                continue;
            }
            let score = usize::from(brief_match) * BRIEF_MATCH_WEIGHT
                + usize::from(profile)
                + usize::from(request.is_none());
            if best.is_none_or(|(best_score, _)| score >= best_score) {
                best = Some((score, index));
            }
        }
        best.map(|(_, index)| index)
    }
}
