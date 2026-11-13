//! Keyboard and mouse handling.

use crate::app::App;
use crate::namer::{NameResult, name_prompt, run_namer};
use crossterm::event::{Event, KeyCode, KeyEventKind, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Position;
use std::{env, thread};
use std::sync::mpsc;

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
pub(crate) fn handle_event(app: &mut App, event: Event, name_tx: &mpsc::Sender<NameResult>) -> bool {
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
