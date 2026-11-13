//! Turning packets into chats: admit a flow, buffer its bytes, and route each
//! response object to the turn it belongs to.

use crate::app::App;
use crate::chat::Chat;
use crate::chat::turn::Turn;
use crate::json::str_field;
use crate::subagent::looks_like_subagent_request;
use crate::wire::{
    Dir, Packet, classify, detect_backend, extract_objects, framed_direction, is_llm_response,
    is_request, looks_like_llm, parse_header, thread_key_from_headers, user_agent_from_headers,
};
use serde_json::Value;
use std::time::Instant;

const MAX_FLOWS: usize = 2000;

impl App {
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
    pub(crate) fn packet(&mut self, packet: Packet) {
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
    pub(crate) fn response(&mut self, flow_key: &str, value: Value) {
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
}
