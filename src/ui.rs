//! Drawing the terminal UI.

pub(crate) mod body;
pub(crate) mod tab_bar;
pub(crate) mod timeline;

use crate::app::App;
use crate::chat::Chat;
use crate::chat::turn::SegKind;
use crate::text::middle_elide;
use crate::transcript::stats_text;
use crate::ui::body::{render_body, segment_anchor_rows, wrapped_rows};
use crate::ui::tab_bar::build_tab_line;
use crate::ui::timeline::timeline_body;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};

const DIM: Style = Style::new().fg(Color::DarkGray);

pub(crate) const TOOL_ORANGE: Color = Color::Rgb(255, 165, 0);

impl SegKind {
    fn color(self) -> Color {
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

/// Columns (or rows) a bordered block spends on its two borders.
const BORDER_CELLS: u16 = 2;

const TAB_LABEL_MAX_CHARS: usize = 22;

pub(crate) fn draw(frame: &mut ratatui::Frame, app: &mut App) {
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
