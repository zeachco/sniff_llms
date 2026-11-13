//! What the user does to tabs: group, merge, close, clean, dump and rename.

use crate::app::App;
use crate::chat::Chat;
use crate::namer::NameResult;
use crate::text::safe_name;
use crate::transcript::render_chat;
use std::env;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

impl App {
    /// Insertion point just after a chat and its already grouped descendants.
    pub(crate) fn group_end(&self, parent_id: &str) -> usize {
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
    pub(crate) fn group(&mut self) {
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
    pub(crate) fn ungroup(&mut self) {
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
    pub(crate) fn merge_prev(&mut self) {
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
    pub(crate) fn close(&mut self) {
        self.close_at(self.selected)
    }
    pub(crate) fn close_at(&mut self, index: usize) {
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
    pub(crate) fn clean_done(&mut self) {
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
    pub(crate) fn dump(&mut self) {
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
    pub(crate) fn rename_commit(&mut self) {
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
    pub(crate) fn apply_name_result(&mut self, (id, result): NameResult) {
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
