//! The tab bar: the window of tabs that fits, with overflow indicators.

use crate::ui::DIM;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

pub(crate) fn build_tab_line(items: &[(String, Style)], selected: usize, available: usize) -> Line<'static> {
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
