//! The body pane: a chat's sections as styled lines, and the display row at
//! which each timeline segment starts in it.

use crate::chat::turn::Turn;
use crate::text::compact_arguments;
use crate::transcript::{Section, turn_sections};
use crate::ui::{Chat, SegKind, TOOL_ORANGE};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};

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

pub(crate) fn render_body(c: &Chat) -> Text<'static> {
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

/// Display rows a logical line takes in an area `width` columns wide: it wraps
/// to ceil(its width / area width) rows, and an empty line still takes one.
/// The scroll range and the timeline anchors both count rows with this, so
/// they cannot disagree.
pub(crate) fn wrapped_rows(line: &Line, width: usize) -> usize {
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
pub(crate) fn segment_anchor_rows(c: &Chat, body: &Text<'static>, width: u16) -> Vec<usize> {
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
