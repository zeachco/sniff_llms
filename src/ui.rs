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
use crate::ui::timeline::{keep_row_visible, timeline_body};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Paragraph, Wrap};

/// The keymap shown by the `?` panel; one wrap-friendly line.
const HELP_TEXT: &str = " ←/→ chat · n next running · ↑/↓ scroll · PgUp/PgDn page · Home top · End follow · wheel body or timeline · click a tab or timeline row · x close · X clean finished · m merge into left · g group · u ungroup · t requests on/off · d dump · a auto-clean · r rename · R ai-name · ? help · q quit ";

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
    // The keymap panel takes the bottom three rows while it is open; the
    // status message has the whole row otherwise.
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(if app.help { 3 } else { 1 }),
        ])
        .split(frame.area());
    let (tab_line, tab_hits) = {
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
    app.tab_area = areas[0];
    app.tab_hits = tab_hits
        .into_iter()
        .map(|(start, end, index)| {
            // The line renders inside the block border.
            (start as u16 + areas[0].x + 1, end as u16 + areas[0].x + 1, index)
        })
        .collect();
    let running = app.chats.iter().filter(|c| c.is_running()).count();
    let title = if running > 0 {
        format!(" llama.cpp conversations · {running} running ")
    } else {
        " llama.cpp conversations ".to_string()
    };
    frame.render_widget(
        Paragraph::new(tab_line).block(Block::bordered().title(title)),
        areas[0],
    );
    let timeline_width = (areas[1].width / 3).clamp(24, 36);
    let content = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(timeline_width), Constraint::Min(1)])
        .split(areas[1]);
    render_chat_panes(frame, app, &content);
    let status = Paragraph::new(status_bar(app, areas[2].width));
    let status = if app.help {
        status.wrap(Wrap { trim: false })
    } else {
        status
    };
    frame.render_widget(status, areas[2]);
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
        app.body_rows = visible;
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
        // The timeline follows the active entry while the body follows the
        // stream, so the underline never leaves the pane; wheel scrolling
        // moves it until the next snap.
        let tl_visible = content[0].height.saturating_sub(BORDER_CELLS) as usize;
        let tl_max = segment_rows.len().saturating_sub(tl_visible) as u16;
        if c.follow && let Some(row) = active {
            c.timeline_scroll = keep_row_visible(row, tl_visible, tl_max);
        }
        c.timeline_scroll = c.timeline_scroll.min(tl_max);
        frame.render_widget(
            Paragraph::new(timeline.text)
                .block(Block::bordered().title(" timeline "))
                .scroll((c.timeline_scroll, 0)),
            content[0],
        );
        // The title shows the paused state, because a scrolled-back body
        // stops moving without any other cue.
        let mut title = vec![Span::raw(format!(
            " {} | {} chunks | {} ",
            c.model,
            c.chunks(),
            c.finish().unwrap_or("streaming")
        ))];
        if !c.follow {
            title.push(Span::styled(
                " paused ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        frame.render_widget(
            Paragraph::new(body)
                .block(Block::bordered().title(Line::from(title)))
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
        app.body_rows = 0;
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

/// The bottom line: the rename editor when active, the keymap while the help
/// is open, the status message otherwise. The message gets the whole row when
/// the help is closed, so long paths and ids are not truncated on narrow
/// terminals; the keymap lives in the `?` panel.
pub(crate) fn status_bar(app: &App, width: u16) -> Text<'static> {
    if let Some(buf) = app.rename.as_ref() {
        return Text::from(Line::from(vec![
            Span::styled(
                " rename ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(" {buf} ")),
            Span::styled("│ Enter=save  Esc=cancel", DIM),
        ]));
    }
    if app.help {
        return Text::from(Line::from(Span::raw(HELP_TEXT)));
    }
    let hint = " ? keys";
    let status = format!(" {}", app.status);
    let pad = width
        .saturating_sub((status.chars().count() + hint.chars().count()) as u16);
    let mut line = vec![Span::raw(status)];
    if pad > 0 {
        line.push(Span::raw(" ".repeat(pad as usize)));
    }
    line.push(Span::styled(hint, DIM));
    Text::from(Line::from(line))
}
