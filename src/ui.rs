//! Drawing the terminal UI.

use crate::chat::Chat;
use crate::chat::turn::SegKind;
use ratatui::style::{Color, Style};

pub(crate) const DIM: Style = Style::new().fg(Color::DarkGray);

pub(crate) const TOOL_ORANGE: Color = Color::Rgb(255, 165, 0);

impl SegKind {
    pub(crate) fn color(self) -> Color {
        match self {
            SegKind::Thinking => Color::DarkGray,
            SegKind::Answer => Color::White,
            SegKind::Tool => TOOL_ORANGE,
        }
    }
}

impl Chat {
    /// Color of the last generated thing, for the tab bar.
    pub(crate) fn state_color(&self) -> Color {
        self.last()
            .segments
            .last()
            .map_or(Color::Green, |s| s.kind.color())
    }
}
