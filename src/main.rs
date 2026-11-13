use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseButton,
        MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    env,
    fs::{self, File},
    io::{self, BufRead, BufReader, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, ExitCode, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

const MAX_FLOWS: usize = 2000;
/// How long a chat that already finished may stay open after its last chunk:
/// the silent gap between two turns of an interactive session, when nothing is
/// on the wire yet but the harness is still in use.
const LIVE_GRACE: Duration = Duration::from_secs(30);

#[derive(Default)]
struct Flow {
    request_buf: String,
    response_buf: String,
    requests: Vec<CapturedRequest>,
    /// Thread key parsed from the current HTTP request's headers, waiting to
    /// be attached to the body object once it is complete.
    pending_thread: Option<String>,
    /// User-Agent of the current HTTP request, waiting to be attached to the
    /// body object once it is complete.
    pending_user_agent: Option<String>,
    server: Option<String>,
}
/// A captured request body plus the explicit thread identity it declared, if
/// any (headers or body fields).
#[derive(Clone)]
struct CapturedRequest {
    body: Value,
    thread: Option<String>,
    user_agent: Option<String>,
}
struct Packet {
    header: String,
    payload: String,
}
#[derive(Default)]
struct ToolCall {
    id: String,
    name: String,
    arguments: String,
}

impl ToolCall {
    fn is_empty(&self) -> bool {
        self.id.is_empty() && self.name.is_empty() && self.arguments.is_empty()
    }
}

/// The three kinds of output a stream can produce. They drive both the
/// timeline entries and the tab color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SegKind {
    Thinking,
    Answer,
    Tool,
}
impl SegKind {
    fn label(self) -> &'static str {
        match self {
            SegKind::Thinking => "thinking",
            SegKind::Answer => "answer",
            SegKind::Tool => "tool",
        }
    }
    fn color(self) -> Color {
        match self {
            SegKind::Thinking => Color::DarkGray,
            SegKind::Answer => Color::White,
            SegKind::Tool => TOOL_ORANGE,
        }
    }
}

/// One contiguous run of output of a single kind, with the wall-clock time it
/// took to generate. Token counts are estimated from characters because the
/// streamed chunks never carry per-chunk token counts.
struct Segment {
    kind: SegKind,
    chars: usize,
    started: Instant,
    ended: Option<Instant>,
    /// For tool segments: index into `Turn::tool_calls` of the call that
    /// generated the segment, so the timeline can anchor to the call's line
    /// in the body panel.
    tool_call: Option<usize>,
}
impl Segment {
    fn tokens(&self) -> usize {
        self.chars.div_ceil(4)
    }
    fn duration(&self) -> Duration {
        self.ended
            .unwrap_or_else(Instant::now)
            .saturating_duration_since(self.started)
    }
}

/// One HTTP response (one response id) inside a conversation. A chat can hold
/// several turns when the client continues after a tool call with a new id.
struct Turn {
    id: String,
    request: Option<Value>,
    reasoning: String,
    answer: String,
    tool_calls: Vec<ToolCall>,
    finish: Option<String>,
    chunks: usize,
    usage: Option<Value>,
    timings: Option<Value>,
    segments: Vec<Segment>,
}
impl Turn {
    fn new(id: String, request: Option<Value>) -> Self {
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

    /// Apply one streamed response chunk to this turn. Returns false when the
    /// chunk carries no `choices[0]` — usage/timings still count, nothing else
    /// does. (Mirrors the early return the caller used to make inline.)
    fn apply_chunk(&mut self, value: &Value) -> bool {
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
        if let Some(calls) = delta
            .and_then(|v| v.get("tool_calls"))
            .and_then(Value::as_array)
        {
            for (pos, call) in calls.iter().enumerate() {
                // Tool-call arguments stream as many small deltas; concatenate them
                // per call (keyed by the delta `index`) instead of one line each.
                let idx = call
                    .get("index")
                    .and_then(Value::as_u64)
                    .map(|i| i as usize)
                    .unwrap_or(pos);
                while self.tool_calls.len() <= idx {
                    self.tool_calls.push(ToolCall::default());
                }
                let mut label = String::new();
                {
                    let tc = &mut self.tool_calls[idx];
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
                }
                if let Some(args) = call.pointer("/function/arguments") {
                    let frag = text_of(args, false);
                    if !frag.is_empty() && !label.is_empty() {
                        label.push(' ');
                    }
                    label.push_str(&frag);
                    self.tool_calls[idx].arguments.push_str(&frag);
                }
                // Count the tool name too, so a call with empty arguments
                // still produces a tool segment (and turns the tab orange).
                self.push_tool(idx, &label);
            }
        }
        if let Some(s) = str_field(choice, "finish_reason") {
            self.finish = Some(s.into());
            self.close_active();
        }
        true
    }
}

struct Chat {
    name: Option<String>,
    model: String,
    backend: Option<String>,
    flow: String,
    /// Explicit thread identity declared by the client (session header or
    /// body field), when it sent one. Authoritative over content heuristics.
    thread: Option<String>,
    /// User-Agent of the client that sent the first observed request.
    /// Identifies the caller (e.g. `pi/1.4.0`) without decoding the body.
    client: Option<String>,
    /// First-turn id of the chat this one was spawned from, when it is a
    /// subagent generation. Such chats are grouped under their parent tab
    /// instead of being merged into its message history.
    parent: Option<String>,
    /// Last time a chunk arrived, used to bound subagent auto-grouping.
    last_activity: Instant,
    turns: Vec<Turn>,
    scroll: u16,
    follow: bool,
}
impl Chat {
    fn new(
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
    fn id(&self) -> &str {
        &self.turns[0].id
    }
    fn last(&self) -> &Turn {
        self.turns
            .last()
            .expect("a chat always has at least one turn")
    }
    fn has_id(&self, id: &str) -> bool {
        self.turns.iter().any(|t| t.id == id)
    }
    fn finish(&self) -> Option<&str> {
        self.last().finish.as_deref()
    }
    /// True while the conversation is still going: the last turn is generating,
    /// or ended asking for tool calls (the client is executing them, or a
    /// subagent is), or produced output inside the live-grace window.
    fn is_running(&self) -> bool {
        match self.finish() {
            // No finish reason: still streaming, or the finish chunk was lost.
            None => true,
            Some("tool_calls") => true,
            Some(_) => self.last_activity.elapsed() < LIVE_GRACE,
        }
    }
    fn chunks(&self) -> usize {
        self.turns.iter().map(|t| t.chunks).sum()
    }
    fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.turns.iter().flat_map(|t| t.tool_calls.iter())
    }
    fn usage(&self) -> Option<&Value> {
        self.turns.iter().rev().find_map(|t| t.usage.as_ref())
    }
    fn timings(&self) -> Option<&Value> {
        self.turns.iter().rev().find_map(|t| t.timings.as_ref())
    }
    /// Tool calls in progress that may have spawned subagent generations. The
    /// turn must have finished asking for tools, and no later request may have
    /// delivered a result for the call yet.
    fn open_subagent_calls(&self) -> Vec<&ToolCall> {
        // `None` means the turn is still streaming: a harness may execute a
        // completed tool call before the finish chunk reaches the wire.
        if !matches!(self.finish(), Some("tool_calls") | None) {
            return Vec::new();
        }
        let resolved: HashSet<&str> = self
            .last()
            .request
            .as_ref()
            .and_then(request_messages)
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
    /// Color of the last generated thing, for the tab bar.
    fn state_color(&self) -> Color {
        self.last()
            .segments
            .last()
            .map_or(Color::Green, |s| s.kind.color())
    }
}
#[derive(Clone, Copy)]
enum AutoClean {
    Never,
    ToolCalls,
    Stop,
    Length,
    ContentFilter,
}
impl AutoClean {
    fn next(self) -> AutoClean {
        match self {
            AutoClean::Never => AutoClean::ToolCalls,
            AutoClean::ToolCalls => AutoClean::Stop,
            AutoClean::Stop => AutoClean::Length,
            AutoClean::Length => AutoClean::ContentFilter,
            AutoClean::ContentFilter => AutoClean::Never,
        }
    }
    fn label(self) -> &'static str {
        match self {
            AutoClean::Never => "off",
            AutoClean::ToolCalls => "tool_calls",
            AutoClean::Stop => "stop",
            AutoClean::Length => "length",
            AutoClean::ContentFilter => "content_filter",
        }
    }
    fn action_for(self, finish: &str) -> Option<String> {
        let armed = !matches!(self, AutoClean::Never);
        (armed && finish == self.label()).then(|| finish.to_string())
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dir {
    Request,
    Response,
}
const DIM: Style = Style::new().fg(Color::DarkGray);
const TOOL_ORANGE: Color = Color::Rgb(255, 165, 0);
/// Weight of a brief echo in the subagent-parent score. A brief match is a
/// much stronger signal than the profile prompt or the timing-only fallback,
/// so it dominates: any brief match outranks any combination without one.
const BRIEF_MATCH_WEIGHT: usize = 10;
struct App {
    flows: HashMap<String, Flow>,
    chats: Vec<Chat>,
    selected: usize,
    ignored: HashSet<String>,
    status: String,
    auto_clean: AutoClean,
    rename: Option<String>,
    /// First-turn id of the chat marked as a manual group parent.
    group_mark: Option<String>,
    /// Last frame's panel geometry and the timeline/body anchors of the
    /// selected chat, so mouse events can map to a timeline row or a body row.
    timeline_area: Rect,
    body_area: Rect,
    /// Timeline content row → flat segment index (None on turn separators).
    timeline_segments: Vec<Option<usize>>,
    /// Flat segment index → display row at which the segment's text starts in
    /// the body panel (after wrapping).
    body_anchors: Vec<usize>,
}

impl App {
    fn new() -> Self {
        Self {
            flows: HashMap::new(),
            chats: vec![],
            selected: 0,
            ignored: HashSet::new(),
            status: "waiting for traffic".into(),
            auto_clean: AutoClean::Never,
            rename: None,
            group_mark: None,
            timeline_area: Rect::default(),
            body_area: Rect::default(),
            timeline_segments: Vec::new(),
            body_anchors: Vec::new(),
        }
    }
    /// Create the flow if absent and record which endpoint is the LLM server.
    fn admit_flow(&mut self, key: &str, direction: Dir, src: &str, dst: &str) {
        let flow = self.flows.entry(key.to_owned()).or_default();
        if flow.server.is_none() {
            flow.server = Some(if direction == Dir::Response {
                src.to_owned()
            } else {
                dst.to_owned()
            });
        }
    }

    fn packet(&mut self, packet: Packet) {
        let Some((src, dst)) = parse_header(&packet.header) else {
            return;
        };
        let mut endpoints = [src.to_owned(), dst.to_owned()];
        endpoints.sort();
        let key = endpoints.join(" <-> ");
        let direction = match framed_direction(&packet.payload) {
            // HTTP framing tells us the direction outright.
            Some(d) => {
                if !looks_like_llm(&packet.payload) {
                    return;
                }
                if !self.flows.contains_key(&key) && self.flows.len() >= MAX_FLOWS {
                    return;
                }
                self.admit_flow(&key, d, src, dst);
                d
            }
            // Unframed body data. A brand-new flow must identify itself as LLM
            // traffic from its content; an existing flow is trusted and the
            // direction comes from the endpoints, because mid-body segments of a
            // large request carry no markers at all.
            None => {
                if let Some(server) = self.flows.get(&key).and_then(|f| f.server.as_deref()) {
                    if src == server {
                        Dir::Response
                    } else if dst == server {
                        Dir::Request
                    } else {
                        return;
                    }
                } else {
                    if !looks_like_llm(&packet.payload) {
                        return;
                    }
                    let Some(d) = classify(&packet.payload) else {
                        return;
                    };
                    if self.flows.len() >= MAX_FLOWS {
                        return;
                    }
                    self.admit_flow(&key, d, src, dst);
                    d
                }
            }
        };
        let response = direction == Dir::Response;
        let objects = {
            let flow = self.flows.entry(key.clone()).or_default();
            // The start of an HTTP request carries its headers. A declared
            // session/conversation id is kept until the body object is parsed,
            // so it can be attached to that request.
            if !response && framed_direction(&packet.payload) == Some(Dir::Request) {
                flow.pending_thread = thread_key_from_headers(&packet.payload);
                flow.pending_user_agent = user_agent_from_headers(&packet.payload);
            }
            let buf = if response {
                &mut flow.response_buf
            } else {
                &mut flow.request_buf
            };
            buf.push_str(&packet.payload);
            buf.push('\n');
            extract_objects(buf)
        };
        for object in objects {
            if !response && is_request(&object) {
                let flow = self.flows.get_mut(&key).expect("flow was just inserted");
                let thread = flow
                    .pending_thread
                    .take()
                    .or_else(|| thread_key_from_body(&object));
                flow.requests.push(CapturedRequest {
                    body: object,
                    thread,
                    user_agent: flow.pending_user_agent.take(),
                });
                if flow.requests.len() > 20 {
                    flow.requests.remove(0);
                }
            } else if response {
                self.response(&key, object);
            }
        }
    }
    fn response(&mut self, flow_key: &str, value: Value) {
        if !is_llm_response(&value) {
            return;
        }
        let Some(id) = str_field(&value, "id") else {
            return;
        };
        if self.ignored.contains(id) {
            return;
        }
        // A router can expose the same stream on several ports. Keep a single
        // chat per response id and let the first observed flow own it. Chunks
        // mirrored on another router leg must not create a second tab or append
        // every streamed token a second time. They can still contribute the
        // client-side request metadata the owning (backend) leg lacks.
        let index = if let Some(index) = self.chats.iter().position(|c| c.has_id(id)) {
            if self.chats[index].flow != flow_key {
                self.adopt_mirrored(index, flow_key);
                return;
            }
            index
        } else {
            let captured = self
                .flows
                .get(flow_key)
                .and_then(|f| f.requests.last())
                .cloned();
            let request = captured.as_ref().map(|c| c.body.clone());
            let client = captured.as_ref().and_then(|c| c.user_agent.clone());
            let thread = captured.and_then(|c| c.thread);
            // A subagent generation is a separate pi process with its own
            // session; never fold it into the parent's message history.
            let subagent = request.as_ref().is_some_and(looks_like_subagent_request);
            // A new response id that continues a previous session after a tool
            // call is another turn of the same conversation: append it instead
            // of opening a new tab.
            let continued = if subagent {
                None
            } else {
                self.continuation(flow_key, request.as_ref(), thread.as_deref())
            };
            if let Some(index) = continued {
                let chat = &mut self.chats[index];
                chat.flow = flow_key.into();
                chat.last_activity = Instant::now();
                chat.turns.push(Turn::new(id.into(), request));
                self.status = format!("continued {} → {id}", chat.id());
                index
            } else {
                let model = str_field(&value, "model")
                    .or_else(|| request.as_ref().and_then(|r| str_field(r, "model")))
                    .unwrap_or("unknown")
                    .into();
                let mut chat = Chat::new(
                    id.into(),
                    request.clone(),
                    model,
                    flow_key.into(),
                    thread,
                    client,
                );
                let parent = self.subagent_parent(request.as_ref());
                let position = match parent {
                    Some(parent_index) => {
                        let parent_id = self.chats[parent_index].id().to_owned();
                        chat.parent = Some(parent_id.clone());
                        self.status = format!("grouped {id} under {parent_id}");
                        self.group_end(&parent_id)
                    }
                    None => {
                        self.status = format!("captured {id}");
                        self.chats.len()
                    }
                };
                let had_chats = !self.chats.is_empty();
                self.chats.insert(position, chat);
                // Keep the selection on the same chat when another one is
                // inserted before it. Appending never moves it.
                if had_chats && position <= self.selected {
                    self.selected += 1;
                }
                position
            }
        };
        let chat = &mut self.chats[index];
        chat.last_activity = Instant::now();
        if let Some(model) = str_field(&value, "model") {
            chat.model = model.into();
        }
        if let Some(backend) = detect_backend(&value) {
            chat.backend = Some(backend.to_string());
        }
        {
            let turn = chat.turns.last_mut().expect("chat has a turn");
            if !turn.apply_chunk(&value) {
                return;
            }
        }
        let auto = {
            let chat = &self.chats[index];
            chat.finish().and_then(|f| self.auto_clean.action_for(f))
        };
        if let Some(reason) = auto {
            let id = self.chats[index].id().to_owned();
            self.close_at(index);
            self.status = format!("auto-cleaned {id} ({reason})");
        }
    }
    /// Find the chat a new response belongs to. A declared thread id is
    /// authoritative. Otherwise the strongest signal is the tool-call id: the
    /// new request echoes the assistant `tool_calls` that the previous turn
    /// emitted, which works even when the previous request was never captured
    /// (capture started late) or when the client rewrites its history between
    /// calls (dynamic system prompts). When no id is available, fall back to a
    /// message-for-message prefix match followed by tool output.
    fn continuation(
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
            .filter_map(|m| m.get("tool_calls").and_then(Value::as_array))
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
                .request
                .as_ref()
                .and_then(request_messages)
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
    /// The client leg of a mirrored stream can carry request metadata the
    /// owning (router backend) leg lacks. Adopt what the chat is missing.
    fn adopt_mirrored(&mut self, index: usize, flow_key: &str) {
        let Some(captured) = self
            .flows
            .get(flow_key)
            .and_then(|f| f.requests.last())
            .cloned()
        else {
            return;
        };
        let chat = &mut self.chats[index];
        if chat.thread.is_none() {
            chat.thread = captured.thread;
        }
        if chat.client.is_none() {
            chat.client = captured.user_agent;
        }
        let turn = chat.turns.last_mut().expect("chat has a turn");
        if turn.request.is_none() {
            turn.request = Some(captured.body);
        }
    }
    /// Find the chat that most likely spawned this generation as a subagent.
    /// A parent qualifies while it waits on an unresolved subagent tool call.
    /// The brief it passed is matched against the child request when captured;
    /// otherwise the most recently active parent wins.
    fn subagent_parent(&self, request: Option<&Value>) -> Option<usize> {
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
            if chat.last_activity.elapsed() > Duration::from_secs(300) {
                continue;
            }
            let brief_match = calls.iter().any(|call| {
                briefs(&call.arguments).iter().any(|brief| {
                    let needle = first_chars(brief, 80);
                    needle.chars().count() >= 40 && plain.contains(&needle)
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
    /// Insertion point just after a chat and its already grouped descendants.
    fn group_end(&self, parent_id: &str) -> usize {
        let Some(parent) = self.chats.iter().position(|c| c.id() == parent_id) else {
            return self.chats.len();
        };
        self.subtree_end(parent)
    }
    fn in_subtree(&self, mut index: usize, root_id: &str) -> bool {
        for _ in 0..self.chats.len() {
            let Some(parent) = self.chats[index].parent.as_deref() else {
                return false;
            };
            if parent == root_id {
                return true;
            }
            let Some(next) = self.chats.iter().position(|c| c.id() == parent) else {
                return false;
            };
            index = next;
        }
        false
    }
    /// Manually group the selected tab under the one on its left. Useful for
    /// conversations that were already split before grouping heuristics could
    /// see them.
    /// Manual grouping is a two-step mark: press `g` on the parent, select the
    /// child, press `g` again. The child's subtree moves right after the
    /// parent's group.
    fn group(&mut self) {
        let Some(chat) = self.chats.get(self.selected) else {
            self.status = "no chat to group".into();
            return;
        };
        let chat_id = chat.id().to_owned();
        let Some(parent_id) = self.group_mark.take() else {
            self.status = format!("group parent {chat_id}: select a child and press g");
            self.group_mark = Some(chat_id);
            return;
        };
        if parent_id == chat_id {
            self.status = "group mark cleared".into();
            return;
        }
        let Some(parent_index) = self.chats.iter().position(|c| c.id() == parent_id) else {
            self.status = "group parent tab was closed".into();
            return;
        };
        if self.in_subtree(parent_index, &chat_id) {
            self.status = "cannot group a chat under its own child".into();
            return;
        }
        let start = self.selected;
        let end = self.subtree_end(start);
        let mut block: Vec<Chat> = self.chats.drain(start..end).collect();
        if let Some(first) = block.first_mut() {
            first.parent = Some(parent_id.clone());
        }
        let position = self.group_end(&parent_id);
        self.chats.splice(position..position, block);
        self.selected = position;
        self.status = format!("grouped under {parent_id}");
    }
    fn subtree_end(&self, index: usize) -> usize {
        let root = self.chats[index].id().to_owned();
        let mut end = index + 1;
        while end < self.chats.len() && self.in_subtree(end, &root) {
            end += 1;
        }
        end
    }
    fn ungroup(&mut self) {
        let Some(chat) = self.chats.get_mut(self.selected) else {
            self.status = "no chat to ungroup".into();
            return;
        };
        self.status = match chat.parent.take() {
            Some(parent) => format!("ungrouped from {parent}"),
            None => "chat is not grouped".into(),
        };
    }
    /// Manually regroup: append the selected chat to the one on its left as a
    /// continuation. A safety net for conversations whose requests and
    /// tool-call ids were both missed by the capture.
    fn merge_prev(&mut self) {
        if self.selected == 0 {
            self.status = "nothing on the left to merge into".into();
            return;
        }
        let chat = self.chats.remove(self.selected);
        let index = self.selected - 1;
        self.selected = index;
        let turns = chat.turns.len();
        let merged_id = chat.id().to_owned();
        if self.group_mark.as_deref() == Some(merged_id.as_str()) {
            self.group_mark = Some(self.chats[index].id().to_owned());
        }
        let target_id = self.chats[index].id().to_owned();
        let target = &mut self.chats[index];
        target.thread = target.thread.take().or(chat.thread);
        target.client = target.client.take().or(chat.client);
        target.turns.extend(chat.turns);
        target.flow = chat.flow;
        for other in self.chats.iter_mut() {
            if other.parent.as_deref() == Some(merged_id.as_str()) {
                other.parent = Some(target_id.clone());
            }
        }
        self.status = format!("merged {turns} turns into {target_id}");
    }
    fn close(&mut self) {
        self.close_at(self.selected)
    }
    fn close_at(&mut self, index: usize) {
        if index >= self.chats.len() {
            return;
        }
        let chat = self.chats.remove(index);
        let closed_id = chat.id().to_owned();
        if self.group_mark.as_deref() == Some(closed_id.as_str()) {
            self.group_mark = None;
        }
        let grandparent = chat.parent.clone();
        for turn in &chat.turns {
            self.ignored.insert(turn.id.clone());
        }
        // Release the capture buffers only when no other chat still lives on
        // that connection, otherwise a chat we keep would lose its pending data.
        let shared = self.chats.iter().any(|c| c.flow == chat.flow);
        if !shared && let Some(flow) = self.flows.get_mut(&chat.flow) {
            flow.requests.clear();
            flow.request_buf.clear();
            flow.response_buf.clear();
        }
        // Do not silently drop subagent tabs when their parent goes away.
        for other in self.chats.iter_mut() {
            if other.parent.as_deref() == Some(closed_id.as_str()) {
                other.parent = grandparent.clone();
            }
        }
        if self.chats.is_empty() {
            self.selected = 0;
        } else {
            if index < self.selected {
                self.selected -= 1;
            }
            self.selected = self.selected.min(self.chats.len() - 1);
        }
        self.status = format!("closed and forgot {closed_id}");
    }
    /// Close every finished chat and keep the running ones. Each drop goes
    /// through `close_at`, so a cleaned chat is still forgotten by id, its flow
    /// buffers are released, and its subagent tabs are reparented instead of
    /// vanishing with it. Reverse order keeps the collected indices valid.
    fn clean_done(&mut self) {
        let done: Vec<usize> = self
            .chats
            .iter()
            .enumerate()
            .filter(|(_, chat)| !chat.is_running())
            .map(|(index, _)| index)
            .rev()
            .collect();
        if done.is_empty() {
            self.status = "nothing finished to clean".into();
            return;
        }
        let cleaned = done.len();
        for index in done {
            self.close_at(index);
        }
        self.status = format!(
            "cleaned {cleaned} finished chat{} · {} running",
            if cleaned == 1 { "" } else { "s" },
            self.chats.len()
        );
    }
    fn dump(&mut self) {
        let Some(chat) = self.chats.get(self.selected) else {
            return;
        };
        let path = env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| ".".into())
            .join(format!("chat-id-{}.log", safe_name(chat.id())));
        self.status =
            match File::create(&path).and_then(|mut f| f.write_all(render_chat(chat).as_bytes())) {
                Ok(()) => format!("dumped {}", path.display()),
                Err(e) => format!("dump failed: {e}"),
            };
    }
    fn rename_commit(&mut self) {
        let buf = self.rename.take().unwrap_or_default();
        let name = buf.trim().to_string();
        if name.is_empty() {
            if let Some(c) = self.chats.get_mut(self.selected) {
                c.name = None;
                self.status = "name cleared".into();
            } else {
                self.status = "no chat to rename".into();
            }
            return;
        }
        if let Some(c) = self.chats.get_mut(self.selected) {
            c.name = Some(name.clone());
            self.status = format!("renamed → {name}");
        } else {
            self.status = "no chat to rename".into();
        }
    }
}

fn parse_header(s: &str) -> Option<(&str, &str)> {
    let mut p = s.split_whitespace();
    (p.next()? == "T").then_some(())?;
    let a = p.next()?;
    (p.next()? == "->").then_some(())?;
    Some((a, p.next()?))
}
fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn str_or<'a>(v: &'a Value, key: &str, default: &'a str) -> &'a str {
    str_field(v, key).unwrap_or(default)
}

fn request_messages<'a>(v: &'a Value) -> Option<&'a [Value]> {
    v.get("messages").and_then(Value::as_array).map(Vec::as_slice)
}

fn text_of(v: &Value, pretty: bool) -> String {
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

fn is_request(v: &Value) -> bool {
    v.get("messages").is_some_and(Value::is_array) || v.get("prompt").is_some_and(Value::is_string)
}
fn is_llm_response(v: &Value) -> bool {
    v.get("id").is_some_and(|v| v.is_string())
        && (v.get("choices").is_some_and(Value::is_array)
            || v.get("completion").is_some()
            || v.get("prompt").is_some_and(Value::is_array))
}
/// Guess the serving backend from response-only signals. `None` means
/// "plain OpenAI-compatible, no distinguishing marker".
fn detect_backend(v: &Value) -> Option<&'static str> {
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
fn looks_like_llm(payload: &str) -> bool {
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

fn thread_key_from_headers(payload: &str) -> Option<String> {
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
fn user_agent_from_headers(payload: &str) -> Option<String> {
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

fn thread_key_from_body(body: &Value) -> Option<String> {
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

/// Tool names a harness uses to spawn a child generation. Cindy/pi's subagent
/// extension uses `subagent`; the others cover common conventions.
fn is_subagent_tool(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "subagent" | "spawn_agent" | "task" | "delegate"
    )
}

/// A child generation carries one of the subagent profile prompts that the
/// spawning extension appends. Normal conversations must not be grouped just
/// because some parent is waiting on a tool.
fn looks_like_subagent_request(request: &Value) -> bool {
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

/// Unescaped text of a request's messages, for substring matching against the
/// brief a parent passed to its subagent tool.
fn request_plain_text(request: &Value) -> String {
    let Some(messages) = request_messages(request) else {
        return String::new();
    };
    let mut out = String::new();
    for message in messages {
        if let Some(content) = message.get("content") {
            out.push_str(&text_of(content, false));
        }
        out.push('\n');
        if out.len() > 256 * 1024 {
            break;
        }
    }
    out
}

/// Long string values embedded in a tool call's JSON arguments: task briefs,
/// prompts and instructions. These are the phrases a child generation echoes
/// back in its own request.
fn briefs(arguments: &str) -> Vec<String> {
    fn walk(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::String(s) if s.chars().count() >= 40 => out.push(s.clone()),
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
fn framed_direction(payload: &str) -> Option<Dir> {
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
fn classify(payload: &str) -> Option<Dir> {
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
fn safe_name(s: &str) -> String {
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

fn middle_elide(s: &str, max_chars: usize) -> String {
    let chars = s.chars().collect::<Vec<_>>();
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
fn compact_arguments(raw: &str) -> String {
    match serde_json::from_str::<Value>(raw) {
        Ok(parsed) => serde_json::to_string(&parsed).unwrap_or_else(|_| raw.to_string()),
        Err(_) => raw.split_whitespace().collect::<Vec<_>>().join(" "),
    }
}

fn number_at<'a>(value: Option<&'a Value>, keys: &[&str]) -> Option<&'a Value> {
    let value = value?;
    keys.iter()
        .find_map(|key| value.get(*key))
        .filter(|v| v.is_number())
}

fn stats_text(c: &Chat) -> String {
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
        .filter(|tc| !tc.id.is_empty() || !tc.name.is_empty() || !tc.arguments.is_empty())
        .count();
    if tool_count > 0 {
        stats.push(format!("Tool calls: {tool_count}"));
    }
    if let Some(usage) = c.usage() {
        let prompt = number_at(Some(usage), &["prompt_tokens"]);
        let completion = number_at(Some(usage), &["completion_tokens"]);
        let total = number_at(Some(usage), &["total_tokens"]);
        if prompt.is_some() || completion.is_some() || total.is_some() {
            stats.push(format!(
                "Tokens: prompt {} | completion {} | total {}",
                prompt.map_or("?".into(), Value::to_string),
                completion.map_or("?".into(), Value::to_string),
                total.map_or("?".into(), Value::to_string),
            ));
        }
    }
    if let Some(timings) = c.timings() {
        let predicted = number_at(
            Some(timings),
            &["predicted_per_second", "tokens_per_second"],
        );
        let prompt = number_at(Some(timings), &["prompt_per_second"]);
        if predicted.is_some() || prompt.is_some() {
            stats.push(format!(
                "Speed: generation {} tok/s | prompt {} tok/s",
                predicted.map_or("?".into(), Value::to_string),
                prompt.map_or("?".into(), Value::to_string),
            ));
        }
    }
    stats.join("\n")
}

// Why hand-rolled: streamed SSE chunks split JSON objects across packet
// boundaries, so serde_json's StreamDeserializer cannot be used incrementally
// here — objects are only complete once brace depth returns to zero inside
// this growing buffer. The 4096-char tail cap keeps a long non-JSON prefix
// from growing the buffer without bound.
fn extract_objects(buffer: &mut String) -> Vec<Value> {
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
        buffer[consumed..]
            .chars()
            .rev()
            .take(4096)
            .collect::<String>()
            .chars()
            .rev()
            .collect()
    };
    out
}
fn messages_text(messages: &[Value], skip: usize) -> String {
    let mut out = String::new();
    for m in messages.iter().skip(skip) {
        let role = str_or(m, "role", "unknown");
        let content = m.get("content").map(|v| text_of(v, true)).unwrap_or_default();
        out.push_str(&format!("[{role}]\n{content}\n"));
        if let Some(calls) = m.get("tool_calls").and_then(Value::as_array) {
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
fn tool_calls_text(turn: &Turn) -> String {
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

/// The sections of one turn, in display order. The single source of the dump
/// and body layout: render_chat, render_body and the anchor math in
/// segment_anchor_rows all consume this, so the layout can only change here.
fn turn_sections(c: &Chat, index: usize) -> Vec<(String, String)> {
    let turn = &c.turns[index];
    let mut sections = Vec::new();
    if c.turns.len() > 1 {
        sections.push((format!("=== TURN {} ===", index + 1), String::new()));
    }
    sections.push(("=== REQUEST ===".into(), turn_request_text(&c.turns, index)));
    sections.push(("=== REASONING ===".into(), turn.reasoning.clone()));
    sections.push(("=== ANSWER ===".into(), turn.answer.clone()));
    if !turn.tool_calls.is_empty() {
        sections.push(("=== TOOL CALLS ===".into(), tool_calls_text(turn)));
    }
    sections
}

fn render_chat(c: &Chat) -> String {
    let mut out = format!("{}\n\n", stats_text(c));
    for index in 0..c.turns.len() {
        for (header, text) in turn_sections(c, index) {
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

fn body_section(out: &mut Vec<Line<'static>>, header: &str, text: &str, style: Style) {
    out.push(Line::from(Span::styled(
        header.to_string(),
        style.add_modifier(Modifier::BOLD),
    )));
    for l in text.lines() {
        out.push(Line::from(Span::styled(l.to_string(), style)));
    }
    out.push(Line::from(""));
}

fn render_body(c: &Chat) -> Text<'static> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let default = Style::default();
    let orange = Style::default().fg(TOOL_ORANGE);
    let cyan = Style::default().fg(Color::Cyan);
    for index in 0..c.turns.len() {
        for (header, text) in turn_sections(c, index) {
            let style = match header.as_str() {
                "=== TOOL CALLS ===" => orange,
                h if h.starts_with("=== TURN ") => cyan,
                _ => default,
            };
            body_section(&mut out, &header, &text, style);
        }
    }
    Text::from(out)
}

fn format_duration(d: Duration) -> String {
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

/// The left-hand column: one entry per generated run of thinking, answer or
/// tool call, in generation order, with its estimated token count and actual
/// wall-clock duration. `segments` maps each content row to the flat index of
/// its segment (turn order, then segment order) or to None on turn separators,
/// so a mouse click on a row can jump the body panel to that segment.
struct Timeline {
    text: Text<'static>,
    segments: Vec<Option<usize>>,
}
fn timeline_body(c: &Chat, active: Option<usize>) -> Timeline {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut segments: Vec<Option<usize>> = Vec::new();
    let dim = DIM;
    let turns = c.turns.len();
    let mut flat = 0usize;
    for (index, turn) in c.turns.iter().enumerate() {
        for segment in &turn.segments {
            let streaming = segment.ended.is_none();
            let marker = if streaming { "▶" } else { "•" };
            let mut style = Style::default().fg(segment.kind.color());
            if streaming {
                style = style.add_modifier(Modifier::BOLD);
            }
            // Underline the entry whose text currently starts the visible body
            // area, so the timeline doubles as a scroll progress indicator.
            if Some(flat) == active {
                style = style.add_modifier(Modifier::UNDERLINED);
            }
            lines.push(Line::from(Span::styled(
                format!(
                    "{marker} {:<8} {:>4}tok {}",
                    segment.kind.label(),
                    segment.tokens(),
                    format_duration(segment.duration())
                ),
                style,
            )));
            segments.push(Some(flat));
            flat += 1;
        }
        if index + 1 < turns {
            lines.push(Line::from(Span::styled(
                format!("  ── turn {} ──", index + 2),
                dim,
            )));
            segments.push(None);
        }
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled("  (no output yet)", dim)));
        segments.push(None);
    }
    Timeline {
        text: Text::from(lines),
        segments,
    }
}

/// Number of newlines in the first `chars` characters of `text`.
fn newlines_before(text: &str, chars: usize) -> usize {
    text.chars().take(chars).filter(|&c| c == '\n').count()
}

/// Line offset, within the TOOL CALLS section text, at which each call's
/// `[id] name` line starts, mirroring the skip order of `tool_calls_text`.
fn tool_call_line_starts(turn: &Turn) -> Vec<usize> {
    let mut starts = vec![usize::MAX; turn.tool_calls.len()];
    let mut at = 0usize;
    for (i, tc) in turn.tool_calls.iter().enumerate() {
        if tc.is_empty() {
            continue;
        }
        starts[i] = at;
        at += 1;
        if !compact_arguments(&tc.arguments).is_empty() {
            at += 1;
        }
    }
    starts
}

/// For every generated segment (turn order, then segment order), the display
/// row — after wrapping at `width` — at which its text starts in the body
/// built by `render_body`. Clicking a timeline row jumps the body scroll to
/// that row; the row that starts the visible area underlines its entry.
// Why the fallback arithmetic: this maps logical body lines to wrapped
// display rows. It walks the same turn_sections list the renderers emit, so
// anchors stay in sync with the visible text by construction; the fallbacks
// (`unwrap_or(row)`, `unwrap_or(0)`) tolerate lines appended after the map
// was built and out-of-range segments instead of panicking.
fn segment_anchor_rows(c: &Chat, body: &Text<'static>, width: u16) -> Vec<usize> {
    let width = width.max(1) as usize;
    // Display row at which each logical line of the body starts. A logical
    // line wraps to ceil(its width / area width) display rows.
    let mut line_rows: Vec<usize> = Vec::with_capacity(body.lines.len());
    let mut row = 0usize;
    for line in &body.lines {
        line_rows.push(row);
        row += line.width().div_ceil(width).max(1);
    }
    let mut out: Vec<usize> = Vec::new();
    for (index, turn) in c.turns.iter().enumerate() {
        // Walk the shared section list exactly as the renderers emit it:
        // each section costs header + text lines + blank, and its text
        // starts one line after its header.
        let mut line_idx = 0usize;
        let mut reasoning_start = 0usize;
        let mut answer_start = 0usize;
        let mut tool_start = None;
        for (header, text) in turn_sections(c, index) {
            let text_start = line_idx + 1;
            match header.as_str() {
                "=== REASONING ===" => reasoning_start = text_start,
                "=== ANSWER ===" => answer_start = text_start,
                "=== TOOL CALLS ===" => tool_start = Some(text_start),
                _ => {}
            }
            line_idx += 2 + text.lines().count();
        }
        // Each kind's accumulated text is the concatenation of that kind's
        // segments in generation order, so a segment's first line sits
        // `newlines_before` past its section's first text line.
        let tool_starts = tool_start.is_some().then(|| tool_call_line_starts(turn));
        let mut thinking_at = 0usize;
        let mut answer_at = 0usize;
        for segment in &turn.segments {
            let line = match segment.kind {
                SegKind::Thinking => {
                    let line = reasoning_start + newlines_before(&turn.reasoning, thinking_at);
                    thinking_at += segment.chars;
                    line
                }
                SegKind::Answer => {
                    let line = answer_start + newlines_before(&turn.answer, answer_at);
                    answer_at += segment.chars;
                    line
                }
                SegKind::Tool => {
                    let call = segment.tool_call.unwrap_or(0);
                    let at = tool_starts
                        .as_ref()
                        .and_then(|starts| starts.get(call).copied())
                        .unwrap_or(0);
                    tool_start.unwrap_or(0) + at
                }
            };
            out.push(line_rows.get(line).copied().unwrap_or(row));
        }
    }
    out
}

fn build_tab_line(items: &[(String, Style)], selected: usize, available: usize) -> Line<'static> {
    let n = items.len();
    if n == 0 {
        return Line::from(" no chats ");
    }
    let sep = " │ ";
    let sep_w = 3usize;
    let widths: Vec<usize> = items.iter().map(|(t, _)| t.chars().count()).collect();
    let ind_text = |count: usize| format!(" +{count}");
    let ind_width = |count: usize| ind_text(count).chars().count();
    let fits = |a: usize, b: usize| -> bool {
        let mut w = 0usize;
        if a > 0 {
            w += ind_width(a) + sep_w;
        }
        w += widths[a..b].iter().sum::<usize>();
        w += sep_w * (b - a - 1);
        if b < n {
            w += sep_w + ind_width(n - b);
        }
        w <= available
    };
    let sel = selected.min(n - 1);
    let mut a = sel;
    let mut b = sel + 1;
    while b < n && fits(a, b + 1) {
        b += 1;
    }
    while a > 0 && fits(a - 1, b) {
        a -= 1;
    }
    let dim = DIM;
    let mut spans: Vec<Span> = Vec::new();
    if a > 0 {
        spans.push(Span::styled(ind_text(a), dim));
        spans.push(Span::raw(sep));
    }
    let sel_text = &items[sel].0;
    let mut first = true;
    for (text, style) in &items[a..b] {
        if !first {
            spans.push(Span::raw(sep));
        }
        first = false;
        let style = if text == sel_text {
            style
                .add_modifier(Modifier::BOLD)
                .add_modifier(Modifier::UNDERLINED)
        } else {
            *style
        };
        spans.push(Span::styled(text.clone(), style));
    }
    if b < n {
        spans.push(Span::raw(sep));
        spans.push(Span::styled(ind_text(n - b), dim));
    }
    Line::from(spans)
}

fn first_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}
fn last_chars(s: &str, n: usize) -> String {
    s.chars()
        .rev()
        .take(n)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}
fn name_prompt(c: &Chat) -> String {
    let first_user = c
        .turns
        .first()
        .and_then(|t| t.request.as_ref())
        .and_then(request_messages)
        .and_then(|m| {
            m.iter()
                .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
                .and_then(|m| m.get("content").and_then(Value::as_str))
                .map(str::to_owned)
        })
        .map(|s| first_chars(&s, 100))
        .unwrap_or_default();
    let tail = last_chars(&c.last().answer, 1000);
    format!(
        "Name this chat conversation in 2-6 words. Reply with ONLY the name, no quotes, no trailing punctuation.\n\nFirst user message (first 100 chars):\n{first_user}\n\nEnd of conversation (last 1000 chars):\n{tail}"
    )
}
fn run_namer(model: &str, prompt: &str) -> Result<String, String> {
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

const USAGE: &str = "Usage: sniff-llms [OPTIONS]\n\n  -i, --interface <name[,name…]>  Capture interface(s); repeatable and/or comma-separated.\n                                   Default: lo0 + the default-route interface on macOS/BSD,\n                                             \"any\" on Linux.\n      --no-elevate                 Do not run ngrep through sudo/pkexec.\n      --dry-run                    Print the resolved capture plan and exit.\n  -h, --help                       Show this help.\n\nPacket capture needs rights the average user does not have: sniff-llms asks\n`pkexec` (Linux desktops) or `sudo` for them once, before the terminal UI takes\nover the screen. On macOS the alternative is the ChmodBPF drop-in, which makes\n/dev/bpf* readable for a group and needs no password at all.\n";

/// Install prefixes searched when a tool is not on `PATH`. `sudo` replaces the
/// inherited `PATH` with `secure_path`, which does not list the Homebrew bin
/// directory, so a brew-installed `ngrep`/`ollama` has to be found by hand.
const BIN_DIRS: [&str; 6] = [
    "/opt/homebrew/bin",
    "/usr/local/bin",
    "/usr/bin",
    "/bin",
    "/usr/sbin",
    "/sbin",
];

fn is_executable(path: &Path) -> bool {
    fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Locate a helper without assuming one distro's layout: `PATH` first, then the
/// usual prefixes (Apple Silicon brew, Intel brew, system). A name containing a
/// `/` is taken as a path and checked as given.
fn resolve_binary(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        let given = PathBuf::from(name);
        return is_executable(&given).then_some(given);
    }
    env::split_paths(&env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
        .or_else(|| {
            BIN_DIRS
                .iter()
                .map(|dir| PathBuf::from(dir).join(name))
                .find(|candidate| is_executable(candidate))
        })
}

fn command_stdout(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(resolve_binary(program)?)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Interfaces this machine can capture on. BSD/macOS libpcap has no `any`
/// pseudo-device, so each one has to be named and one capture per interface is
/// the only way to watch more than a single leg.
fn available_interfaces() -> Vec<String> {
    let listed = if cfg!(target_os = "linux") {
        fs::read_dir("/sys/class/net")
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default()
    } else {
        command_stdout("ifconfig", &["-l"]).unwrap_or_default()
    };
    listed.split_whitespace().map(str::to_owned).collect()
}

/// What to capture when the caller did not name anything: loopback (where a
/// local router lives) plus whatever carries the default route (where LAN
/// clients live). Linux gets both from the `any` pseudo-device in one capture.
fn default_interfaces() -> Vec<String> {
    if cfg!(target_os = "linux") {
        return vec!["any".into()];
    }
    let mut out = vec!["lo0".to_string()];
    if let Some(route) = command_stdout("route", &["-n", "get", "default"])
        && let Some(name) = route
            .lines()
            .find_map(|l| l.trim().strip_prefix("interface:").map(str::trim))
        && !name.is_empty()
        && !out.iter().any(|iface| iface == name)
    {
        out.push(name.to_string());
    }
    out
}

/// `--interface` values: repeatable, and each value may itself be a comma-
/// separated list. First occurrence wins.
fn requested_interfaces(args: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg != "--interface" && arg != "-i" {
            continue;
        }
        let Some(value) = iter.next() else { continue };
        for name in value.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if !out.iter().any(|seen| seen == name) {
                out.push(name.to_string());
            }
        }
    }
    out
}

/// Reject interface names the machine does not have, with the list of names it
/// does: on macOS `any` is the single most common mistake, and `ngrep` answers
/// it with an opaque `ioctl: Operation not permitted`.
fn check_interfaces(mut wanted: Vec<String>, explicit: bool) -> io::Result<Vec<String>> {
    let known = available_interfaces();
    if known.is_empty() {
        return Ok(wanted);
    }
    let usable = |name: &str| {
        (name == "any" && cfg!(target_os = "linux")) || known.iter().any(|iface| iface == name)
    };
    let unknown = wanted
        .iter()
        .filter(|name| !usable(name))
        .cloned()
        .collect::<Vec<_>>();
    if unknown.is_empty() {
        return Ok(wanted);
    }
    if explicit {
        return Err(io::Error::other(format!(
            "no such capture interface: {}\n  available: {}",
            unknown.join(", "),
            known.join(" ")
        )));
    }
    wanted.retain(|name| usable(name));
    if wanted.is_empty() {
        return Err(io::Error::other(format!(
            "no usable capture interface\n  available: {}",
            known.join(" ")
        )));
    }
    Ok(wanted)
}

/// How capture rights are obtained. `pkexec` is the polkit helper shipped by
/// Linux desktops and is absent on macOS, which has only `sudo`; a machine with
/// the ChmodBPF drop-in needs neither.
enum Elevation {
    None,
    Pkexec(PathBuf),
    Sudo(PathBuf),
}

fn runs_as_root() -> bool {
    command_stdout("id", &["-u"]).is_some_and(|uid| uid.trim() == "0")
}

fn can_capture_unaided() -> bool {
    if runs_as_root() {
        return true;
    }
    // macOS hands /dev/bpf* to root only unless the ChmodBPF group rule is
    // installed, and group membership is what matters, so opening one for
    // read/write is the only honest test.
    cfg!(any(target_os = "macos", target_os = "ios"))
        && fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/bpf0")
            .is_ok()
}

/// Linux capture rights are a capability (`CAP_NET_RAW`), not a uid, and can be
/// granted in ways this program cannot see without libc: `setcap` on the
/// `ngrep` binary itself, or an ambient capability from a systemd unit. So on
/// Linux, non-root, the first attempt is made unprivileged and only a fast
/// death promotes it to `pkexec`/`sudo`.
fn try_unprivileged_first() -> bool {
    cfg!(target_os = "linux") && !runs_as_root()
}

fn choose_elevation() -> Elevation {
    if can_capture_unaided() {
        return Elevation::None;
    }
    if let Some(pkexec) = resolve_binary("pkexec") {
        return Elevation::Pkexec(pkexec);
    }
    if let Some(sudo) = resolve_binary("sudo") {
        return Elevation::Sudo(sudo);
    }
    Elevation::None
}

impl Elevation {
    fn describe(&self) -> String {
        match self {
            Elevation::None => "none (already allowed)".into(),
            Elevation::Pkexec(path) => format!("{}", path.display()),
            Elevation::Sudo(path) => format!("{} -n", path.display()),
        }
    }

    /// argv in front of `ngrep`. Refreshing the `sudo` timestamp here is the
    /// point: the password prompt lands on the normal terminal, before the TUI
    /// owns the screen and while `ngrep`'s stdout is still a pipe that would
    /// swallow any later prompt.
    fn argv_prefix(&self) -> io::Result<Vec<String>> {
        match self {
            Elevation::None => Ok(Vec::new()),
            Elevation::Pkexec(path) => Ok(vec![path.display().to_string()]),
            Elevation::Sudo(path) => {
                let granted = Command::new(path)
                    .arg("-v")
                    .status()
                    .is_ok_and(|status| status.success());
                if !granted {
                    return Err(io::Error::other(
                        "`sudo` did not grant capture rights. Either run the built binary yourself:\n  \
                         sudo ./target/release/sniff-llms\n  or install the ChmodBPF drop-in so your user can open /dev/bpf* without a password.",
                    ));
                }
                Ok(vec![path.display().to_string(), "-n".into()])
            }
        }
    }
}

/// One `ngrep` per interface: BSD libpcap has no multi-device capture, so the
/// loopback and LAN legs arrive as separate processes feeding one channel.
fn spawn_ngrep(
    prefix: &[String],
    binary: &Path,
    interface: &str,
    tx: mpsc::Sender<Packet>,
    err_tx: mpsc::Sender<String>,
) -> io::Result<Child> {
    let program = binary.to_string_lossy().into_owned();
    let mut cmd = Command::new(prefix.first().map(String::as_str).unwrap_or(&program));
    let mut child = cmd
        .args(prefix.iter().skip(1))
        .arg(&program)
        .args(["-q", "-l", "-u", "-d", interface, "-W", "byline", "", "tcp"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(stderr) = child.stderr.take() {
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if !line.trim().is_empty() {
                    let _ = err_tx.send(line);
                }
            }
        });
    }
    let stdout = child.stdout.take().unwrap();
    thread::spawn(move || {
        let mut header = String::new();
        let mut payload = String::new();
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if line == "####" {
                if !header.is_empty() {
                    let _ = tx.send(Packet {
                        header: std::mem::take(&mut header),
                        payload: std::mem::take(&mut payload),
                    });
                }
            } else if line.starts_with("T ") && line.contains(" -> ") {
                if !header.is_empty() {
                    let _ = tx.send(Packet { header, payload });
                }
                header = line;
                payload = String::new();
            } else {
                payload.push_str(&line);
                payload.push('\n');
            }
        }
        if !header.is_empty() {
            let _ = tx.send(Packet { header, payload });
        }
    });
    Ok(child)
}

/// How long a capture has to stay alive to count as working. `ngrep` opens the
/// device before it reads anything, so a bad interface or missing rights shows
/// up well inside this window.
const CAPTURE_PROBE: Duration = Duration::from_millis(500);

/// Start every capture and hand back the processes, the packet channel, and
/// whatever the capture helpers complained about.
///
/// The order of attempts matters: unprivileged where that may be enough (Linux
/// capabilities), then `pkexec`/`sudo`. `ngrep` exits at once when it cannot
/// open the device, so a probe window decides whether to keep or escalate.
fn start_capture(
    interfaces: &[String],
    elevation: &Elevation,
    elevate: bool,
) -> io::Result<(Vec<Child>, Receiver<Packet>, Receiver<String>)> {
    let requested = env::var("NGREP").unwrap_or_else(|_| "ngrep".into());
    let binary = resolve_binary(&requested).ok_or_else(|| {
        io::Error::other(format!(
            "`{requested}` not found; install it (`brew install ngrep`, `apt install ngrep`, `pacman -S ngrep`) or point NGREP at it"
        ))
    })?;
    let mut attempts: Vec<bool> = Vec::new();
    if !elevate || matches!(elevation, Elevation::None) {
        attempts.push(false);
    } else {
        if try_unprivileged_first() {
            attempts.push(false);
        }
        attempts.push(true);
    }
    let mut failures = String::new();
    for elevated in attempts {
        let prefix = if elevated {
            elevation.argv_prefix()?
        } else {
            Vec::new()
        };
        let (tx, rx) = mpsc::channel();
        let (err_tx, err_rx) = mpsc::channel();
        let mut captures = Vec::new();
        let mut attempt_failure = String::new();
        for interface in interfaces {
            match spawn_ngrep(&prefix, &binary, interface, tx.clone(), err_tx.clone()) {
                Ok(child) => captures.push(child),
                Err(err) => attempt_failure.push_str(&format!("  {interface}: {err}\n")),
            }
        }
        drop(tx);
        drop(err_tx);
        if !captures.is_empty() {
            thread::sleep(CAPTURE_PROBE);
            let (running, detail) = reap(&mut captures, &err_rx);
            attempt_failure.push_str(&detail);
            if running > 0 {
                // Some legs may still be missing; say so while the terminal is
                // still a normal one instead of leaving a silently partial view.
                if !attempt_failure.is_empty() {
                    eprintln!("sniff-llms: partial capture:\n{attempt_failure}");
                }
                return Ok((captures, rx, err_rx));
            }
            for capture in captures.iter_mut() {
                let _ = capture.kill();
            }
            failures.push_str(&attempt_failure);
        }
    }
    Err(io::Error::other(format!(
        "could not capture on {}:\n{failures}  check the interface name (`ifconfig -l`, `ls /sys/class/net`) and the capture rights",
        interfaces.join(", ")
    )))
}

/// Collect the captures that have exited and return `(still running, what they
/// said)`. `ngrep` reports device errors on stderr and exits immediately, which
/// a full-screen UI would hide behind a quietly empty chat list.
fn reap(captures: &mut [Child], errors: &Receiver<String>) -> (usize, String) {
    let mut detail = String::new();
    while let Ok(line) = errors.try_recv() {
        detail.push_str("ngrep: ");
        detail.push_str(&line);
        detail.push('\n');
    }
    let mut running = 0;
    for child in captures.iter_mut() {
        if child.try_wait().ok().flatten().is_none() {
            running += 1;
        }
    }
    (running, detail)
}

fn draw(frame: &mut ratatui::Frame, app: &mut App) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(frame.area());
    let tab_line = {
        let items: Vec<(String, Style)> = app
            .chats
            .iter()
            .map(|c| {
                // Tab color tracks the last generated thing: orange for a tool
                // call, gray for thinking, white for an answer.
                let marker = if c.finish().is_none() { "●" } else { "✓" };
                let label = c.name.clone().unwrap_or_else(|| c.id().to_owned());
                let prefix = if c.parent.is_some() { "↳ " } else { "" };
                (
                    format!(" {prefix}{} {} ", middle_elide(&label, 22), marker),
                    Style::default().fg(c.state_color()),
                )
            })
            .collect();
        build_tab_line(
            &items,
            app.selected,
            areas[0].width.saturating_sub(2) as usize,
        )
    };
    frame.render_widget(
        Paragraph::new(tab_line).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" llama.cpp conversations "),
        ),
        areas[0],
    );
    let timeline_width = (areas[1].width / 3).clamp(24, 36);
    let content = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(timeline_width), Constraint::Min(1)])
        .split(areas[1]);
    render_chat_panes(frame, app, &content);
    frame.render_widget(Paragraph::new(status_bar(app)), areas[2]);
}

/// Render the timeline/stats/body panes for the selected chat, and record the
/// pane geometry for mouse events.
fn render_chat_panes(frame: &mut ratatui::Frame, app: &mut App, content: &[Rect]) {
    if let Some(c) = app.chats.get_mut(app.selected) {
        let stats = stats_text(c);
        let stats_height = stats
            .lines()
            .count()
            .saturating_add(2)
            .min(u16::MAX as usize) as u16;
        let details = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(stats_height), Constraint::Min(1)])
            .split(content[1]);
        frame.render_widget(
            Paragraph::new(stats).block(Block::default().borders(Borders::ALL).title(" stats ")),
            details[0],
        );
        let body = render_body(c);
        let inner_width = details[1].width.saturating_sub(2) as usize;
        let visible = details[1].height.saturating_sub(2) as usize;
        // The body wraps at its inner width, so the scroll range must be
        // measured in wrapped display rows, not logical lines.
        let body_rows = body
            .lines
            .iter()
            .map(|line| line.width().div_ceil(inner_width.max(1)).max(1))
            .sum::<usize>();
        let max = body_rows.saturating_sub(visible).min(u16::MAX as usize) as u16;
        if c.follow {
            c.scroll = max
        }
        // The entry whose anchor starts the visible area is the scroll
        // position marker of the timeline.
        let anchors = segment_anchor_rows(c, &body, inner_width as u16);
        let top = c.scroll.min(max) as usize;
        let active = anchors.iter().rposition(|&anchor| anchor <= top);
        let timeline = timeline_body(c, active);
        let segment_rows = timeline.segments;
        frame.render_widget(
            Paragraph::new(timeline.text)
                .block(Block::default().borders(Borders::ALL).title(" timeline ")),
            content[0],
        );
        frame.render_widget(
            Paragraph::new(body)
                .block(Block::default().borders(Borders::ALL).title(format!(
                    " {} | {} chunks | {} ",
                    c.model,
                    c.chunks(),
                    c.finish().unwrap_or("streaming")
                )))
                .wrap(Wrap { trim: false })
                .scroll((c.scroll, 0)),
            details[1],
        );
        // Keep the geometry and anchors for mouse events; `c` is not used
        // below, so the borrow of `app.chats` is over.
        app.timeline_area = content[0];
        app.body_area = details[1];
        app.timeline_segments = segment_rows;
        app.body_anchors = anchors;
    } else {
        app.timeline_area = Rect::default();
        app.body_area = Rect::default();
        app.timeline_segments.clear();
        app.body_anchors.clear();
        frame.render_widget(
            Paragraph::new("(no chat)")
                .block(Block::default().borders(Borders::ALL).title(" timeline ")),
            content[0],
        );
        frame.render_widget(
            Paragraph::new("No conversations captured yet.")
                .block(Block::default().borders(Borders::ALL)),
            content[1],
        );
    }
}

/// The bottom status line: rename editor when active, keymap otherwise.
fn status_bar(app: &App) -> Line<'static> {
    match app.rename.as_ref() {
        Some(buf) => Line::from(vec![
            Span::styled(
                " rename ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(" {buf} ")),
            Span::styled(
                "│ Enter=save  Esc=cancel",
                DIM,
            ),
        ]),
        None => {
            let key = |k: &'static str| Span::styled(k, Style::default().fg(Color::Cyan));
            Line::from(vec![
                Span::raw(format!(" {}  ", app.status)),
                key("←/→"),
                Span::raw(" tabs "),
                key("↑/↓"),
                Span::raw(" scroll "),
                key("x"),
                Span::raw(" close "),
                key("X"),
                Span::raw(" clean done "),
                key("m"),
                Span::raw(" merge "),
                key("g"),
                Span::raw(" group "),
                key("u"),
                Span::raw(" ungroup "),
                key("d"),
                Span::raw(" dump "),
                key("a"),
                Span::raw(format!(" auto-clean:{} ", app.auto_clean.label())),
                key("r"),
                Span::raw(" rename "),
                key("R"),
                Span::raw(" name "),
                key("q"),
                Span::raw(" quit"),
            ])
        }
    }
}

fn restore(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) {
    let _ = disable_raw_mode();
    let _ = execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    );
    let _ = terminal.show_cursor();
}

/// Handle one terminal event. Returns false when the app should quit.
fn handle_event(
    app: &mut App,
    event: Event,
    name_tx: &mpsc::Sender<(String, Result<String, String>)>,
) -> bool {
    match event {
        Event::Mouse(mouse) => {
            match mouse.kind {
                // A click on the timeline jumps the body to that segment.
                MouseEventKind::Down(MouseButton::Left)
                    if app.timeline_area.contains(Position {
                        x: mouse.column,
                        y: mouse.row,
                    }) =>
                {
                    let row =
                        (mouse.row as usize).saturating_sub(app.timeline_area.y as usize + 1);
                    if let Some(&Some(flat)) = app.timeline_segments.get(row)
                        && let Some(&anchor) = app.body_anchors.get(flat)
                        && let Some(c) = app.chats.get_mut(app.selected)
                    {
                        c.follow = false;
                        c.scroll = anchor.min(u16::MAX as usize) as u16;
                    }
                }
                // The wheel over the body scrolls it (and stops following).
                MouseEventKind::ScrollUp
                    if app.body_area.contains(Position {
                        x: mouse.column,
                        y: mouse.row,
                    }) =>
                {
                    if let Some(c) = app.chats.get_mut(app.selected) {
                        c.follow = false;
                        c.scroll = c.scroll.saturating_sub(3);
                    }
                }
                MouseEventKind::ScrollDown
                    if app.body_area.contains(Position {
                        x: mouse.column,
                        y: mouse.row,
                    }) =>
                {
                    if let Some(c) = app.chats.get_mut(app.selected) {
                        c.scroll = c.scroll.saturating_add(3);
                    }
                }
                _ => {}
            }
            return true;
        }
        Event::Key(k) => {
            if k.kind != KeyEventKind::Press {
                return true;
            }
            if app.rename.is_some() {
                match k.code {
                    KeyCode::Char(c) => {
                        if let Some(buf) = app.rename.as_mut() {
                            buf.push(c)
                        }
                    }
                    KeyCode::Backspace => {
                        if let Some(buf) = app.rename.as_mut() {
                            buf.pop();
                        }
                    }
                    KeyCode::Enter => app.rename_commit(),
                    KeyCode::Esc => {
                        app.rename = None;
                        app.status = "rename cancelled".into();
                    }
                    _ => {}
                }
                return true;
            }
            match k.code {
                KeyCode::Char('q') => return false,
                KeyCode::Left if !app.chats.is_empty() => {
                    app.selected = app.selected.checked_sub(1).unwrap_or(app.chats.len() - 1)
                }
                KeyCode::Right if !app.chats.is_empty() => {
                    app.selected = (app.selected + 1) % app.chats.len()
                }
                KeyCode::Up => {
                    let Some(c) = app.chats.get_mut(app.selected) else {
                        return true;
                    };
                    c.follow = false;
                    c.scroll = c.scroll.saturating_sub(1)
                }
                KeyCode::Down => {
                    let Some(c) = app.chats.get_mut(app.selected) else {
                        return true;
                    };
                    c.scroll = c.scroll.saturating_add(1)
                }
                KeyCode::End => {
                    let Some(c) = app.chats.get_mut(app.selected) else {
                        return true;
                    };
                    c.follow = true
                }
                KeyCode::Char('x') => app.close(),
                KeyCode::Char('X') => app.clean_done(),
                KeyCode::Char('m') => app.merge_prev(),
                KeyCode::Char('g') => app.group(),
                KeyCode::Char('u') => app.ungroup(),
                KeyCode::Char('d') => app.dump(),
                KeyCode::Char('a') => {
                    app.auto_clean = app.auto_clean.next();
                    app.status = format!("auto-clean: {}", app.auto_clean.label())
                }
                KeyCode::Char('r') => {
                    if app.chats.is_empty() {
                        app.status = "no chat to rename".into()
                    } else {
                        app.rename = Some(String::new());
                        app.status = "rename".into()
                    }
                }
                KeyCode::Char('R') => {
                    let Some(c) = app.chats.get(app.selected) else {
                        app.status = "no chat to name".into();
                        return true;
                    };
                    let prompt = name_prompt(c);
                    let id = c.id().to_owned();
                    let model = env::var("SNIFF_NAMER").unwrap_or_else(|_| "gemma4:e2b".into());
                    let tx = name_tx.clone();
                    app.status = format!("ai-naming with {model}…");
                    thread::spawn(move || {
                        let res = run_namer(&model, &prompt);
                        let _ = tx.send((id, res));
                    });
                }
                _ => {}
            }
        }
        _ => return true,
    }
    true
}
/// Capture failures are written as sentences rather than left to the default
/// `Termination`, which prints `io::Error` in debug form and buries the advice.
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("sniff-llms: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = env::args().collect::<Vec<_>>();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return Ok(());
    }
    let requested = requested_interfaces(&args);
    let explicit = !requested.is_empty();
    let interfaces = check_interfaces(
        if explicit {
            requested
        } else {
            default_interfaces()
        },
        explicit,
    )?;
    let elevation = choose_elevation();
    let elevate = !args.iter().any(|a| a == "--no-elevate");
    if args.iter().any(|a| a == "--dry-run") {
        let helper = env::var("NGREP").unwrap_or_else(|_| "ngrep".into());
        println!("sniff-llms capture plan");
        println!("  interfaces: {}", interfaces.join(", "));
        println!(
            "  capture:    {}",
            resolve_binary(&helper)
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| format!("{helper} (not found)"))
        );
        println!(
            "  elevation:  {}",
            if elevate {
                elevation.describe()
            } else {
                "none (--no-elevate)".into()
            }
        );
        return Ok(());
    }
    let (mut captures, rx, err_rx) = start_capture(&interfaces, &elevation, elevate)?;
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    let mut app = App::new();
    let (name_tx, name_rx) = mpsc::channel::<(String, Result<String, String>)>();
    let mut last_reap = Instant::now();
    let result = loop {
        while let Ok(p) = rx.try_recv() {
            app.packet(p)
        }
        if last_reap.elapsed() >= Duration::from_secs(2) {
            last_reap = Instant::now();
            let (running, detail) = reap(&mut captures, &err_rx);
            if running == 0 {
                let detail = detail.trim().replace('\n', " · ");
                app.status = if detail.is_empty() {
                    "capture stopped".into()
                } else {
                    format!("capture stopped: {detail}")
                };
            }
        }
        while let Ok((id, res)) = name_rx.try_recv() {
            match res {
                Ok(name) => {
                    if let Some(c) = app.chats.iter_mut().find(|c| c.id() == id) {
                        c.name = Some(name.clone());
                        app.status = format!("named → {name}");
                    } else {
                        app.status = "ai-name: chat no longer open".into();
                    }
                }
                Err(e) => app.status = format!("ai-name failed: {e}"),
            }
        }
        terminal.draw(|f| draw(f, &mut app))?;
        if !event::poll(Duration::from_millis(33))? {
            continue;
        }
        if !handle_event(&mut app, event::read()?, &name_tx) {
            break Ok(());
        }
    };
    for capture in captures.iter_mut() {
        let _ = capture.kill();
    }
    restore(&mut terminal);
    result
}

#[cfg(test)]
#[path = "main_spec.rs"]
mod tests;
