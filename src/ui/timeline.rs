//! The timeline pane: one row per thinking / answer / tool segment.

use crate::text::format_duration;
use crate::ui::{Chat, DIM};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};

/// The left-hand column: one entry per generated run of thinking, answer or
/// tool call, in generation order, with its estimated token count and actual
/// wall-clock duration. `segments` maps each content row to the flat index of
/// its segment (turn order, then segment order) or to None on turn separators,
/// so a mouse click on a row can jump the body panel to that segment.
pub(crate) struct Timeline {
    pub(crate) text: Text<'static>,
    pub(crate) segments: Vec<Option<usize>>,
}

pub(crate) fn timeline_body(c: &Chat, active: Option<usize>) -> Timeline {
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
