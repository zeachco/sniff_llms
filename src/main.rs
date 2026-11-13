mod app;
mod capture;
mod chat;
mod flow;
mod input;
mod json;
mod namer;
mod subagent;
mod text;
mod transcript;
mod ui;
mod wire;

use crate::app::App;
use crate::capture::{
    Elevation, check_interfaces, choose_elevation, default_interfaces, reap, requested_interfaces,
    resolve_binary, start_capture,
};
use crate::input::handle_event;
use crate::namer::NameResult;
use crate::ui::draw;
use crossterm::{event, execute};
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::{env, io};
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::{Duration, Instant};

const USAGE: &str = "Usage: sniff-llms [OPTIONS]\n\n  -i, --interface <name[,name…]>  Capture interface(s); repeatable and/or comma-separated.\n                                   Default: lo0 + the default-route interface on macOS/BSD,\n                                             \"any\" on Linux.\n      --no-elevate                 Do not run ngrep through sudo/pkexec.\n      --dry-run                    Print the resolved capture plan and exit.\n  -h, --help                       Show this help.\n\nPacket capture needs rights the average user does not have: sniff-llms asks\n`pkexec` (Linux desktops) or `sudo` for them once, before the terminal UI takes\nover the screen. On macOS the alternative is the ChmodBPF drop-in, which makes\n/dev/bpf* readable for a group and needs no password at all.\n";

fn restore(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) {
    let _ = disable_raw_mode();
    let _ = execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    );
    let _ = terminal.show_cursor();
}

/// Capture failures are written as sentences rather than left to the default
/// `Termination`, which prints `io::Error` in debug form and buries the advice.
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("sniff-llms: {err}");
            ExitCode::FAILURE
        }
    }
}

/// What `--dry-run` shows: the capture that would start, without starting it.
fn print_capture_plan(interfaces: &[String], elevation: &Elevation, elevate: bool) {
    let helper = env::var("NGREP").unwrap_or_else(|_| "ngrep".into());
    println!("sniff-llms capture plan");
    println!("  interfaces: {}", interfaces.join(", "));
    println!(
        "  capture:    {}",
        resolve_binary(&helper)
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| format!("{helper} (not found)"))
    );
    println!(
        "  elevation:  {}",
        if elevate {
            elevation.describe()
        } else {
            "none (--no-elevate)".into()
        }
    );
}
/// How often the capture processes are checked for an early death.
const REAP_INTERVAL: Duration = Duration::from_secs(2);
/// How long the loop waits for input before redrawing.
const REDRAW_TICK: Duration = Duration::from_millis(33);

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = env::args().collect::<Vec<_>>();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return Ok(());
    }
    let requested = requested_interfaces(&args);
    let explicit = !requested.is_empty();
    let interfaces = check_interfaces(
        if explicit {
            requested
        } else {
            default_interfaces()
        },
        explicit,
    )?;
    let elevation = choose_elevation();
    let elevate = !args.iter().any(|a| a == "--no-elevate");
    if args.iter().any(|a| a == "--dry-run") {
        print_capture_plan(&interfaces, &elevation, elevate);
        return Ok(());
    }
    let (mut captures, rx, err_rx) = start_capture(&interfaces, &elevation, elevate)?;
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    let mut app = App::new();
    let (name_tx, name_rx) = mpsc::channel::<NameResult>();
    let mut last_reap = Instant::now();
    let result = loop {
        while let Ok(p) = rx.try_recv() {
            app.packet(p)
        }
        if last_reap.elapsed() >= REAP_INTERVAL {
            last_reap = Instant::now();
            let (running, detail) = reap(&mut captures, &err_rx);
            if running == 0 {
                let detail = detail.trim().replace('\n', " · ");
                app.status = if detail.is_empty() {
                    "capture stopped".into()
                } else {
                    format!("capture stopped: {detail}")
                };
            }
        }
        while let Ok(named) = name_rx.try_recv() {
            app.apply_name_result(named);
        }
        terminal.draw(|f| draw(f, &mut app))?;
        if !event::poll(REDRAW_TICK)? {
            continue;
        }
        if !handle_event(&mut app, event::read()?, &name_tx) {
            break Ok(());
        }
    };
    for capture in captures.iter_mut() {
        let _ = capture.kill();
    }
    restore(&mut terminal);
    result
}

#[cfg(test)]
#[path = "main_spec.rs"]
mod tests;
