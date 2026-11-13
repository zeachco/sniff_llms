//! One response of a conversation: its streamed text, its tool calls and the
//! timeline segments they arrive in.

use crate::chat::{Duration, Instant};
use crate::json::{request_messages, str_field, text_of, tool_calls_of};
use serde_json::Value;

#[derive(Default)]
pub(crate) struct ToolCall {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) arguments: String,
}

impl ToolCall {
    pub(crate) fn is_empty(&self) -> bool {
        self.id.is_empty() && self.name.is_empty() && self.arguments.is_empty()
    }
}

/// The three kinds of output a stream can produce. They drive both the
/// timeline entries and the tab color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SegKind {
    Thinking,
    Answer,
    Tool,
}

impl SegKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            SegKind::Thinking => "thinking",
            SegKind::Answer => "answer",
            SegKind::Tool => "tool",
        }
    }
}

/// One contiguous run of output of a single kind, with the wall-clock time it
/// took to generate. Token counts are estimated from characters because the
/// streamed chunks never carry per-chunk token counts.
pub(crate) struct Segment {
    pub(crate) kind: SegKind,
    pub(crate) chars: usize,
    started: Instant,
    pub(crate) ended: Option<Instant>,
    /// For tool segments: index into `Turn::tool_calls` of the call that
    /// generated the segment, so the timeline can anchor to the call's line
    /// in the body panel.
    pub(crate) tool_call: Option<usize>,
}

impl Segment {
    pub(crate) fn tokens(&self) -> usize {
        self.chars.div_ceil(4)
    }
    pub(crate) fn duration(&self) -> Duration {
        self.ended
            .unwrap_or_else(Instant::now)
            .saturating_duration_since(self.started)
    }
}

/// One HTTP response (one response id) inside a conversation. A chat can hold
/// several turns when the client continues after a tool call with a new id.
pub(crate) struct Turn {
    pub(crate) id: String,
    pub(crate) request: Option<Value>,
    pub(crate) reasoning: String,
    pub(crate) answer: String,
    pub(crate) tool_calls: Vec<ToolCall>,
    pub(crate) finish: Option<String>,
    pub(crate) chunks: usize,
    pub(crate) usage: Option<Value>,
    pub(crate) timings: Option<Value>,
    pub(crate) segments: Vec<Segment>,
}

impl Turn {
    pub(crate) fn new(id: String, request: Option<Value>) -> Self {
        Self {
            id,
            request,
            reasoning: String::new(),
            answer: String::new(),
            tool_calls: Vec::new(),
            finish: None,
            chunks: 0,
            usage: None,
            timings: None,
            segments: Vec::new(),
        }
    }
    /// Extend the active segment of `kind`, or close the previous one and start
    /// a new segment when the kind changes.
    fn push(&mut self, kind: SegKind, text: &str) {
        let chars = text.chars().count();
        if chars == 0 {
            return;
        }
        if let Some(last) = self.segments.last_mut()
            && last.kind == kind
            && last.ended.is_none()
        {
            last.chars += chars;
            return;
        }
        if let Some(last) = self.segments.last_mut() {
            last.ended.get_or_insert_with(Instant::now);
        }
        self.segments.push(Segment {
            kind,
            chars,
            started: Instant::now(),
            ended: None,
            tool_call: None,
        });
    }
    /// Like `push`, but records which tool call generated the text. Consecutive
    /// deltas of one call (or several calls in one delta) merge into a single
    /// segment that keeps the index of the first call.
    fn push_tool(&mut self, call: usize, text: &str) {
        let fresh = self
            .segments
            .last()
            .is_none_or(|s| s.kind != SegKind::Tool || s.ended.is_some());
        self.push(SegKind::Tool, text);
        if fresh && let Some(last) = self.segments.last_mut() {
            last.tool_call = Some(call);
        }
    }
    fn close_active(&mut self) {
        if let Some(last) = self.segments.last_mut() {
            last.ended.get_or_insert_with(Instant::now);
        }
    }

    /// The messages of the request that produced this turn, if it was captured.
    pub(crate) fn messages(&self) -> Option<&[Value]> {
        self.request.as_ref().and_then(request_messages)
    }
    /// Tool-call arguments stream as many small deltas; concatenate them per
    /// call (keyed by the delta `index`, else its position) instead of one
    /// line each.
    fn apply_tool_delta(&mut self, pos: usize, call: &Value) {
        let idx = call
            .get("index")
            .and_then(Value::as_u64)
            .map_or(pos, |i| i as usize);
        while self.tool_calls.len() <= idx {
            self.tool_calls.push(ToolCall::default());
        }
        let tc = &mut self.tool_calls[idx];
        let mut label = String::new();
        if tc.id.is_empty() {
            tc.id = str_field(call, "id").map(str::to_owned).unwrap_or_default();
        }
        if tc.name.is_empty() {
            tc.name = call
                .pointer("/function/name")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_default();
            label.push_str(&tc.name);
        }
        if let Some(args) = call.pointer("/function/arguments") {
            let frag = text_of(args, false);
            if !frag.is_empty() && !label.is_empty() {
                label.push(' ');
            }
            label.push_str(&frag);
            tc.arguments.push_str(&frag);
        }
        // Count the tool name too, so a call with empty arguments
        // still produces a tool segment (and turns the tab orange).
        self.push_tool(idx, &label);
    }
    /// Apply one streamed response chunk to this turn. Returns false when the
    /// chunk carries no `choices[0]` — usage/timings still count, nothing else
    /// does.
    pub(crate) fn apply_chunk(&mut self, value: &Value) -> bool {
        if let Some(usage) = value.get("usage") {
            self.usage = Some(usage.clone());
        }
        if let Some(timings) = value.get("timings") {
            self.timings = Some(timings.clone());
        }
        self.chunks += 1;
        let Some(choice) = value.pointer("/choices/0") else {
            return false;
        };
        let delta = choice.get("delta").or_else(|| choice.get("message"));
        if let Some(s) = delta.and_then(|v| str_field(v, "reasoning_content")) {
            self.reasoning.push_str(s);
            self.push(SegKind::Thinking, s);
        }
        if let Some(s) = delta.and_then(|v| str_field(v, "content")) {
            self.answer.push_str(s);
            self.push(SegKind::Answer, s);
        }
        if let Some(calls) = delta.and_then(tool_calls_of) {
            for (pos, call) in calls.iter().enumerate() {
                self.apply_tool_delta(pos, call);
            }
        }
        if let Some(s) = str_field(choice, "finish_reason") {
            self.finish = Some(s.into());
            self.close_active();
        }
        true
    }
}
