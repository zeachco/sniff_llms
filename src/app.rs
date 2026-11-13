//! The application state: every open chat, the flows feeding them, and what
//! the user has selected.

mod ingest;
mod tabs;
mod threading;

use crate::chat::Chat;
use crate::flow::Flow;
use ratatui::layout::Rect;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy)]
pub(crate) enum AutoClean {
    Never,
    ToolCalls,
    Stop,
    Length,
    ContentFilter,
}

impl AutoClean {
    pub(crate) fn next(self) -> AutoClean {
        match self {
            AutoClean::Never => AutoClean::ToolCalls,
            AutoClean::ToolCalls => AutoClean::Stop,
            AutoClean::Stop => AutoClean::Length,
            AutoClean::Length => AutoClean::ContentFilter,
            AutoClean::ContentFilter => AutoClean::Never,
        }
    }
    pub(crate) fn label(self) -> &'static str {
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

pub(crate) struct App {
    pub(crate) flows: HashMap<String, Flow>,
    pub(crate) chats: Vec<Chat>,
    pub(crate) selected: usize,
    pub(crate) ignored: HashSet<String>,
    pub(crate) status: String,
    pub(crate) auto_clean: AutoClean,
    pub(crate) rename: Option<String>,
    /// First-turn id of the chat marked as a manual group parent.
    pub(crate) group_mark: Option<String>,
    /// Last frame's panel geometry and the timeline/body anchors of the
    /// selected chat, so mouse events can map to a timeline row or a body row.
    pub(crate) timeline_area: Rect,
    pub(crate) body_area: Rect,
    /// Timeline content row → flat segment index (None on turn separators).
    pub(crate) timeline_segments: Vec<Option<usize>>,
    /// Flat segment index → display row at which the segment's text starts in
    /// the body panel (after wrapping).
    pub(crate) body_anchors: Vec<usize>,
}

impl App {
    pub(crate) fn new() -> Self {
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
    /// Index of the chat whose first turn has this id.
    fn index_of(&self, id: &str) -> Option<usize> {
        self.chats.iter().position(|c| c.id() == id)
    }
}
