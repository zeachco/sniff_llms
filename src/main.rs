mod capture;
mod chat;
mod flow;
mod json;
mod namer;
mod subagent;
mod text;
mod transcript;
mod ui;
mod wire;

use crate::chat::Chat;
use crate::flow::Flow;
use crate::namer::{name_prompt, NameResult, run_namer};
use serde_json::Value;
use crate::transcript::{render_chat, Section, stats_text, turn_sections};
use crate::subagent::{briefs, looks_like_subagent_request, MIN_BRIEF_CHARS, request_plain_text};
use crate::chat::turn::{SegKind, Turn};
use crate::ui::{DIM, TOOL_ORANGE};
use crate::wire::{
    classify, detect_backend, Dir, extract_objects, framed_direction, is_llm_response, is_request,
    looks_like_llm, Packet, parse_header, thread_key_from_body, thread_key_from_headers,
    user_agent_from_headers,
};
use crate::text::{
    compact_arguments, first_chars, format_duration, last_chars, middle_elide, safe_name,
};
use crate::json::{number_at, request_messages, str_field, str_or, text_of, tool_calls_of};
use capture::{
    Elevation, check_interfaces, choose_elevation, default_interfaces, reap, requested_interfaces,
    resolve_binary, start_capture,
};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseButton,
        MouseEvent, MouseEventKind,
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
    widgets::{Block, Paragraph, Wrap},
};
use std::{
    collections::{HashMap, HashSet},
    env,
    fs::File,
    io::{self, Write},
    path::PathBuf,
    process::ExitCode,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

const MAX_FLOWS: usize = 2000;

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
/// Weight of a brief echo in the subagent-parent score. A brief match is a
/// much stronger signal than the profile prompt or the timing-only fallback,
/// so it dominates: any brief match outranks any combination without one.
const BRIEF_MATCH_WEIGHT: usize = 10;
/// A parent that has been silent this long is no longer waiting on a subagent.
const SUBAGENT_WINDOW: Duration = Duration::from_secs(300);
/// How much of a brief's opening is searched for in the child request.
const BRIEF_NEEDLE_CHARS: usize = 80;
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

    /// Which way a packet travels, admitting its flow when it is new. `None`
    /// means the packet is not LLM traffic (or the flow table is full).
    fn resolve_direction(
        &mut self,
        key: &str,
        src: &str,
        dst: &str,
        payload: &str,
        framed: Option<Dir>,
    ) -> Option<Dir> {
        // Unframed body data on an existing flow is trusted and the direction
        // comes from the endpoints, because mid-body segments of a large
        // request carry no markers at all.
        if framed.is_none()
            && let Some(server) = self.flows.get(key).and_then(|f| f.server.as_deref())
        {
            return if src == server {
                Some(Dir::Response)
            } else if dst == server {
                Some(Dir::Request)
            } else {
                None
            };
        }
        // Anything else must identify itself as LLM traffic from its content:
        // HTTP framing tells the direction outright, a bare body by its shape.
        if !looks_like_llm(payload) {
            return None;
        }
        let direction = framed.or_else(|| classify(payload))?;
        if !self.flows.contains_key(key) && self.flows.len() >= MAX_FLOWS {
            return None;
        }
        self.admit_flow(key, direction, src, dst);
        Some(direction)
    }

    fn packet(&mut self, packet: Packet) {
        let Some((src, dst)) = parse_header(&packet.header) else {
            return;
        };
        let mut endpoints = [src.to_owned(), dst.to_owned()];
        endpoints.sort();
        let key = endpoints.join(" <-> ");
        let framed = framed_direction(&packet.payload);
        let Some(direction) = self.resolve_direction(&key, src, dst, &packet.payload, framed)
        else {
            return;
        };
        let flow = self
            .flows
            .get_mut(&key)
            .expect("resolve_direction only returns a direction for an admitted flow");
        if direction == Dir::Response {
            flow.response_buf.push_str(&packet.payload);
            flow.response_buf.push('\n');
            for object in extract_objects(&mut flow.response_buf) {
                self.response(&key, object);
            }
            return;
        }
        // The start of an HTTP request carries its headers. A declared
        // session/conversation id is kept until the body object is parsed,
        // so it can be attached to that request.
        if framed == Some(Dir::Request) {
            flow.pending_thread = thread_key_from_headers(&packet.payload);
            flow.pending_user_agent = user_agent_from_headers(&packet.payload);
        }
        flow.request_buf.push_str(&packet.payload);
        flow.request_buf.push('\n');
        for object in extract_objects(&mut flow.request_buf) {
            if is_request(&object) {
                flow.capture_request(object);
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
        let index = match self.chats.iter().position(|c| c.has_id(id)) {
            Some(index) if self.chats[index].flow != flow_key => {
                self.adopt_mirrored(index, flow_key);
                return;
            }
            Some(index) => index,
            None => self.open_turn(flow_key, id, &value),
        };
        let chat = &mut self.chats[index];
        chat.last_activity = Instant::now();
        if let Some(model) = str_field(&value, "model") {
            chat.model = model.into();
        }
        if let Some(backend) = detect_backend(&value) {
            chat.backend = Some(backend.to_string());
        }
        let turn = chat.turns.last_mut().expect("chat has a turn");
        if !turn.apply_chunk(&value) {
            return;
        }
        let auto = self.chats[index]
            .finish()
            .and_then(|f| self.auto_clean.action_for(f));
        if let Some(reason) = auto {
            let id = self.chats[index].id().to_owned();
            self.close_at(index);
            self.status = format!("auto-cleaned {id} ({reason})");
        }
    }
    /// A response id seen for the first time starts a turn: the next one of the
    /// chat it continues, or the first of a new chat. Returns the chat's index.
    fn open_turn(&mut self, flow_key: &str, id: &str, value: &Value) -> usize {
        let captured = self
            .flows
            .get(flow_key)
            .and_then(|f| f.requests.last())
            .cloned();
        let (request, thread, client) = match captured {
            Some(c) => (Some(c.body), c.thread, c.user_agent),
            None => (None, None, None),
        };
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
            return index;
        }
        let model = str_field(value, "model")
            .or_else(|| request.as_ref().and_then(|r| str_field(r, "model")))
            .unwrap_or("unknown")
            .into();
        let parent = self.subagent_parent(request.as_ref());
        let chat = Chat::new(id.into(), request, model, flow_key.into(), thread, client);
        self.place_chat(chat, parent)
    }
    /// Insert a new chat at the end of its parent's group, or as the last tab.
    fn place_chat(&mut self, mut chat: Chat, parent: Option<usize>) -> usize {
        let id = chat.id().to_owned();
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
    /// Insertion point just after a chat and its already grouped descendants.
    /// Index of the chat whose first turn has this id.
    fn index_of(&self, id: &str) -> Option<usize> {
        self.chats.iter().position(|c| c.id() == id)
    }
    fn group_end(&self, parent_id: &str) -> usize {
        let Some(parent) = self.index_of(parent_id) else {
            return self.chats.len();
        };
        self.subtree_end(parent)
    }
    fn in_subtree(&self, mut index: usize, root_id: &str) -> bool {
        // why: a parent chain is at most chats.len() long. The bound keeps a
        // parent cycle from looping forever: merge_prev re-points parents and
        // can leave a chat as its own ancestor.
        for _ in 0..self.chats.len() {
            let Some(parent) = self.chats[index].parent.as_deref() else {
                return false;
            };
            if parent == root_id {
                return true;
            }
            let Some(next) = self.index_of(parent) else {
                return false;
            };
            index = next;
        }
        false
    }
    /// Manual grouping is a two-step mark: press `g` on the parent, select the
    /// child, press `g` again. The child's subtree moves right after the
    /// parent's group. Useful for conversations that were already split before
    /// the grouping heuristics could see them.
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
        let Some(parent_index) = self.index_of(&parent_id) else {
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
        let name = self.rename.take().unwrap_or_default().trim().to_string();
        let Some(c) = self.chats.get_mut(self.selected) else {
            self.status = "no chat to rename".into();
            return;
        };
        if name.is_empty() {
            c.name = None;
            self.status = "name cleared".into();
        } else {
            self.status = format!("renamed → {name}");
            c.name = Some(name);
        }
    }
    /// The namer thread answered for the chat it was started on.
    fn apply_name_result(&mut self, (id, result): NameResult) {
        self.status = match (result, self.index_of(&id)) {
            (Err(e), _) => format!("ai-name failed: {e}"),
            (Ok(_), None) => "ai-name: chat no longer open".into(),
            (Ok(name), Some(index)) => {
                let status = format!("named → {name}");
                self.chats[index].name = Some(name);
                status
            }
        };
    }
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
        for (section, header, text) in turn_sections(c, index) {
            let style = match section {
                Section::ToolCalls => orange,
                Section::Turn => cyan,
                Section::Request | Section::Reasoning | Section::Answer => default,
            };
            body_section(&mut out, &header, &text, style);
        }
    }
    Text::from(out)
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
                DIM,
            )));
            segments.push(None);
        }
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled("  (no output yet)", DIM)));
        segments.push(None);
    }
    Timeline {
        text: Text::from(lines),
        segments,
    }
}

/// Display rows a logical line takes in an area `width` columns wide: it wraps
/// to ceil(its width / area width) rows, and an empty line still takes one.
/// The scroll range and the timeline anchors both count rows with this, so
/// they cannot disagree.
fn wrapped_rows(line: &Line, width: usize) -> usize {
    line.width().div_ceil(width.max(1)).max(1)
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
    // Display row at which each logical line of the body starts.
    let mut line_rows: Vec<usize> = Vec::with_capacity(body.lines.len());
    let mut row = 0usize;
    for line in &body.lines {
        line_rows.push(row);
        row += wrapped_rows(line, width as usize);
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
        for (section, _, text) in turn_sections(c, index) {
            let text_start = line_idx + 1;
            match section {
                Section::Reasoning => reasoning_start = text_start,
                Section::Answer => answer_start = text_start,
                Section::ToolCalls => tool_start = Some(text_start),
                Section::Turn | Section::Request => {}
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
    let mut spans: Vec<Span> = Vec::new();
    if a > 0 {
        spans.push(Span::styled(ind_text(a), DIM));
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
        spans.push(Span::styled(ind_text(n - b), DIM));
    }
    Line::from(spans)
}

const USAGE: &str = "Usage: sniff-llms [OPTIONS]\n\n  -i, --interface <name[,name…]>  Capture interface(s); repeatable and/or comma-separated.\n                                   Default: lo0 + the default-route interface on macOS/BSD,\n                                             \"any\" on Linux.\n      --no-elevate                 Do not run ngrep through sudo/pkexec.\n      --dry-run                    Print the resolved capture plan and exit.\n  -h, --help                       Show this help.\n\nPacket capture needs rights the average user does not have: sniff-llms asks\n`pkexec` (Linux desktops) or `sudo` for them once, before the terminal UI takes\nover the screen. On macOS the alternative is the ChmodBPF drop-in, which makes\n/dev/bpf* readable for a group and needs no password at all.\n";

/// Columns (or rows) a bordered block spends on its two borders.
const BORDER_CELLS: u16 = 2;
const TAB_LABEL_MAX_CHARS: usize = 22;

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
                    format!(" {prefix}{} {} ", middle_elide(&label, TAB_LABEL_MAX_CHARS), marker),
                    Style::default().fg(c.state_color()),
                )
            })
            .collect();
        build_tab_line(
            &items,
            app.selected,
            areas[0].width.saturating_sub(BORDER_CELLS) as usize,
        )
    };
    frame.render_widget(
        Paragraph::new(tab_line).block(
            Block::bordered()
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
            .saturating_add(BORDER_CELLS as usize)
            .min(u16::MAX as usize) as u16;
        let details = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(stats_height), Constraint::Min(1)])
            .split(content[1]);
        frame.render_widget(
            Paragraph::new(stats).block(Block::bordered().title(" stats ")),
            details[0],
        );
        let body = render_body(c);
        let inner_width = details[1].width.saturating_sub(BORDER_CELLS) as usize;
        let visible = details[1].height.saturating_sub(BORDER_CELLS) as usize;
        // The body wraps at its inner width, so the scroll range must be
        // measured in wrapped display rows, not logical lines.
        let body_rows = body
            .lines
            .iter()
            .map(|line| wrapped_rows(line, inner_width))
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
                .block(Block::bordered().title(" timeline ")),
            content[0],
        );
        frame.render_widget(
            Paragraph::new(body)
                .block(Block::bordered().title(format!(
                    " {} | {} chunks | {} ",
                    c.model,
                    c.chunks(),
                    c.finish().unwrap_or("streaming")
                )))
                .wrap(Wrap { trim: false })
                .scroll((c.scroll, 0)),
            details[1],
        );
        // Keep the geometry and anchors for mouse events.
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
                .block(Block::bordered().title(" timeline ")),
            content[0],
        );
        frame.render_widget(
            Paragraph::new("No conversations captured yet.")
                .block(Block::bordered()),
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
/// Rows the body moves per mouse-wheel notch.
const WHEEL_ROWS: u16 = 3;

fn handle_mouse(app: &mut App, mouse: MouseEvent) {
    let at = Position {
        x: mouse.column,
        y: mouse.row,
    };
    let over_body = app.body_area.contains(at);
    match mouse.kind {
        // A click on the timeline jumps the body to that segment.
        MouseEventKind::Down(MouseButton::Left) if app.timeline_area.contains(at) => {
            let row = (mouse.row as usize).saturating_sub(app.timeline_area.y as usize + 1);
            if let Some(&Some(flat)) = app.timeline_segments.get(row)
                && let Some(&anchor) = app.body_anchors.get(flat)
                && let Some(c) = app.chats.get_mut(app.selected)
            {
                c.follow = false;
                c.scroll = anchor.min(u16::MAX as usize) as u16;
            }
        }
        // The wheel over the body scrolls it.
        MouseEventKind::ScrollUp if over_body => {
            if let Some(c) = app.chats.get_mut(app.selected) {
                c.scroll_up(WHEEL_ROWS);
            }
        }
        MouseEventKind::ScrollDown if over_body => {
            if let Some(c) = app.chats.get_mut(app.selected) {
                c.scroll_down(WHEEL_ROWS);
            }
        }
        _ => {}
    }
}
/// Keys while the rename prompt is open.
fn handle_rename_key(app: &mut App, code: KeyCode) {
    let Some(buf) = app.rename.as_mut() else {
        return;
    };
    match code {
        KeyCode::Char(c) => buf.push(c),
        KeyCode::Backspace => {
            buf.pop();
        }
        KeyCode::Enter => app.rename_commit(),
        KeyCode::Esc => {
            app.rename = None;
            app.status = "rename cancelled".into();
        }
        _ => {}
    }
}
/// Ask the local model for a name; the answer comes back on `name_tx`.
fn start_ai_naming(app: &mut App, name_tx: &mpsc::Sender<NameResult>) {
    let Some(c) = app.chats.get(app.selected) else {
        app.status = "no chat to name".into();
        return;
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
/// Returns false when the user quits.
fn handle_event(app: &mut App, event: Event, name_tx: &mpsc::Sender<NameResult>) -> bool {
    let code = match event {
        Event::Mouse(mouse) => {
            handle_mouse(app, mouse);
            return true;
        }
        Event::Key(k) if k.kind == KeyEventKind::Press => k.code,
        _ => return true,
    };
    if app.rename.is_some() {
        handle_rename_key(app, code);
        return true;
    }
    match code {
        KeyCode::Char('q') => return false,
        KeyCode::Left if !app.chats.is_empty() => {
            app.selected = app.selected.checked_sub(1).unwrap_or(app.chats.len() - 1)
        }
        KeyCode::Right if !app.chats.is_empty() => {
            app.selected = (app.selected + 1) % app.chats.len()
        }
        KeyCode::Up => {
            if let Some(c) = app.chats.get_mut(app.selected) {
                c.scroll_up(1)
            }
        }
        KeyCode::Down => {
            if let Some(c) = app.chats.get_mut(app.selected) {
                c.scroll_down(1)
            }
        }
        KeyCode::End => {
            if let Some(c) = app.chats.get_mut(app.selected) {
                c.follow = true
            }
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
        KeyCode::Char('R') => start_ai_naming(app, name_tx),
        _ => {}
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

/// What `--dry-run` shows: the capture that would start, without starting it.
fn print_capture_plan(interfaces: &[String], elevation: &Elevation, elevate: bool) {
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
}
/// How often the capture processes are checked for an early death.
const REAP_INTERVAL: Duration = Duration::from_secs(2);
/// How long the loop waits for input before redrawing.
const REDRAW_TICK: Duration = Duration::from_millis(33);

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
        print_capture_plan(&interfaces, &elevation, elevate);
        return Ok(());
    }
    let (mut captures, rx, err_rx) = start_capture(&interfaces, &elevation, elevate)?;
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    let mut app = App::new();
    let (name_tx, name_rx) = mpsc::channel::<NameResult>();
    let mut last_reap = Instant::now();
    let result = loop {
        while let Ok(p) = rx.try_recv() {
            app.packet(p)
        }
        if last_reap.elapsed() >= REAP_INTERVAL {
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
        while let Ok(named) = name_rx.try_recv() {
            app.apply_name_result(named);
        }
        terminal.draw(|f| draw(f, &mut app))?;
        if !event::poll(REDRAW_TICK)? {
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
