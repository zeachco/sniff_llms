//! Getting packets off the wire: which interfaces to watch, where the helper
//! binaries live, how to obtain capture rights, and the `ngrep` processes that
//! feed `Packet`s to the UI.

use crate::wire::Packet;
use std::{
    env, fs,
    io::{self, BufRead, BufReader},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::Duration,
};

/// Install prefixes searched when a tool is not on `PATH`. `sudo` replaces the
/// inherited `PATH` with `secure_path`, which does not list the Homebrew bin
/// directory, so a brew-installed `ngrep`/`ollama` has to be found by hand.
const BIN_DIRS: [&str; 6] = [
    "/opt/homebrew/bin",
    "/usr/local/bin",
    "/usr/bin",
    "/bin",
    "/usr/sbin",
    "/sbin",
];

fn is_executable(path: &Path) -> bool {
    fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Locate a helper without assuming one distro's layout: `PATH` first, then the
/// usual prefixes (Apple Silicon brew, Intel brew, system). A name containing a
/// `/` is taken as a path and checked as given.
pub(crate) fn resolve_binary(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        let given = PathBuf::from(name);
        return is_executable(&given).then_some(given);
    }
    env::split_paths(&env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
        .or_else(|| {
            BIN_DIRS
                .iter()
                .map(|dir| PathBuf::from(dir).join(name))
                .find(|candidate| is_executable(candidate))
        })
}

fn command_stdout(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(resolve_binary(program)?)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Interfaces this machine can capture on. BSD/macOS libpcap has no `any`
/// pseudo-device, so each one has to be named and one capture per interface is
/// the only way to watch more than a single leg.
fn available_interfaces() -> Vec<String> {
    if cfg!(target_os = "linux") {
        return fs::read_dir("/sys/class/net")
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
    }
    command_stdout("ifconfig", &["-l"])
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

/// What to capture when the caller did not name anything: loopback (where a
/// local router lives) plus whatever carries the default route (where LAN
/// clients live). Linux gets both from the `any` pseudo-device in one capture.
pub(crate) fn default_interfaces() -> Vec<String> {
    if cfg!(target_os = "linux") {
        return vec!["any".into()];
    }
    let mut out = vec!["lo0".to_string()];
    if let Some(route) = command_stdout("route", &["-n", "get", "default"])
        && let Some(name) = route
            .lines()
            .find_map(|l| l.trim().strip_prefix("interface:").map(str::trim))
        && !name.is_empty()
        && !out.iter().any(|iface| iface == name)
    {
        out.push(name.to_string());
    }
    out
}

/// `--interface` values: repeatable, and each value may itself be a comma-
/// separated list. First occurrence wins.
pub(crate) fn requested_interfaces(args: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg != "--interface" && arg != "-i" {
            continue;
        }
        let Some(value) = iter.next() else { continue };
        for name in value.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if !out.iter().any(|seen| seen == name) {
                out.push(name.to_string());
            }
        }
    }
    out
}

/// Reject interface names the machine does not have, with the list of names it
/// does: on macOS `any` is the single most common mistake, and `ngrep` answers
/// it with an opaque `ioctl: Operation not permitted`.
pub(crate) fn check_interfaces(mut wanted: Vec<String>, explicit: bool) -> io::Result<Vec<String>> {
    let known = available_interfaces();
    if known.is_empty() {
        return Ok(wanted);
    }
    let usable = |name: &str| {
        (name == "any" && cfg!(target_os = "linux")) || known.iter().any(|iface| iface == name)
    };
    let unknown = wanted
        .iter()
        .filter(|name| !usable(name))
        .cloned()
        .collect::<Vec<_>>();
    if unknown.is_empty() {
        return Ok(wanted);
    }
    if explicit {
        return Err(io::Error::other(format!(
            "no such capture interface: {}\n  available: {}",
            unknown.join(", "),
            known.join(" ")
        )));
    }
    wanted.retain(|name| usable(name));
    if wanted.is_empty() {
        return Err(io::Error::other(format!(
            "no usable capture interface\n  available: {}",
            known.join(" ")
        )));
    }
    Ok(wanted)
}

/// How capture rights are obtained. `pkexec` is the polkit helper shipped by
/// Linux desktops and is absent on macOS, which has only `sudo`; a machine with
/// the ChmodBPF drop-in needs neither.
pub(crate) enum Elevation {
    None,
    Pkexec(PathBuf),
    Sudo(PathBuf),
}

fn runs_as_root() -> bool {
    command_stdout("id", &["-u"]).is_some_and(|uid| uid.trim() == "0")
}

fn can_capture_unaided() -> bool {
    if runs_as_root() {
        return true;
    }
    // macOS hands /dev/bpf* to root only unless the ChmodBPF group rule is
    // installed, and group membership is what matters, so opening one for
    // read/write is the only honest test.
    cfg!(any(target_os = "macos", target_os = "ios"))
        && fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/bpf0")
            .is_ok()
}

/// Linux capture rights are a capability (`CAP_NET_RAW`), not a uid, and can be
/// granted in ways this program cannot see without libc: `setcap` on the
/// `ngrep` binary itself, or an ambient capability from a systemd unit. So on
/// Linux, non-root, the first attempt is made unprivileged and only a fast
/// death promotes it to `pkexec`/`sudo`.
fn try_unprivileged_first() -> bool {
    cfg!(target_os = "linux") && !runs_as_root()
}

pub(crate) fn choose_elevation() -> Elevation {
    if can_capture_unaided() {
        return Elevation::None;
    }
    if let Some(pkexec) = resolve_binary("pkexec") {
        return Elevation::Pkexec(pkexec);
    }
    if let Some(sudo) = resolve_binary("sudo") {
        return Elevation::Sudo(sudo);
    }
    Elevation::None
}

impl Elevation {
    pub(crate) fn describe(&self) -> String {
        match self {
            Elevation::None => "none (already allowed)".into(),
            Elevation::Pkexec(path) => format!("{}", path.display()),
            Elevation::Sudo(path) => format!("{} -n", path.display()),
        }
    }

    /// argv in front of `ngrep`. Refreshing the `sudo` timestamp here is the
    /// point: the password prompt lands on the normal terminal, before the TUI
    /// owns the screen and while `ngrep`'s stdout is still a pipe that would
    /// swallow any later prompt.
    fn argv_prefix(&self) -> io::Result<Vec<String>> {
        match self {
            Elevation::None => Ok(Vec::new()),
            Elevation::Pkexec(path) => Ok(vec![path.display().to_string()]),
            Elevation::Sudo(path) => {
                let granted = Command::new(path)
                    .arg("-v")
                    .status()
                    .is_ok_and(|status| status.success());
                if !granted {
                    return Err(io::Error::other(
                        "`sudo` did not grant capture rights. Either run the built binary yourself:\n  \
                         sudo ./target/release/sniff-llms\n  or install the ChmodBPF drop-in so your user can open /dev/bpf* without a password.",
                    ));
                }
                Ok(vec![path.display().to_string(), "-n".into()])
            }
        }
    }
}

/// Group `ngrep -W byline` output into packets: a `T src -> dst` header line
/// followed by its payload lines, with `####` between packets.
fn frame_packets(lines: impl Iterator<Item = String>, tx: mpsc::Sender<Packet>) {
    let mut header = String::new();
    let mut payload = String::new();
    for line in lines {
        if line == "####" {
            if !header.is_empty() {
                let _ = tx.send(Packet {
                    header: std::mem::take(&mut header),
                    payload: std::mem::take(&mut payload),
                });
            }
        } else if line.starts_with("T ") && line.contains(" -> ") {
            if !header.is_empty() {
                let _ = tx.send(Packet { header, payload });
            }
            header = line;
            payload = String::new();
        } else {
            payload.push_str(&line);
            payload.push('\n');
        }
    }
    if !header.is_empty() {
        let _ = tx.send(Packet { header, payload });
    }
}

/// One `ngrep` per interface: BSD libpcap has no multi-device capture, so the
/// loopback and LAN legs arrive as separate processes feeding one channel.
fn spawn_ngrep(
    prefix: &[String],
    binary: &Path,
    interface: &str,
    tx: mpsc::Sender<Packet>,
    err_tx: mpsc::Sender<String>,
) -> io::Result<Child> {
    let program = binary.to_string_lossy().into_owned();
    let mut cmd = Command::new(prefix.first().map(String::as_str).unwrap_or(&program));
    let mut child = cmd
        .args(prefix.iter().skip(1))
        .arg(&program)
        .args(["-q", "-l", "-u", "-d", interface, "-W", "byline", "", "tcp"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(stderr) = child.stderr.take() {
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if !line.trim().is_empty() {
                    let _ = err_tx.send(line);
                }
            }
        });
    }
    let stdout = child.stdout.take().unwrap();
    thread::spawn(move || {
        frame_packets(BufReader::new(stdout).lines().map_while(Result::ok), tx)
    });
    Ok(child)
}

/// How long a capture has to stay alive to count as working. `ngrep` opens the
/// device before it reads anything, so a bad interface or missing rights shows
/// up well inside this window.
const CAPTURE_PROBE: Duration = Duration::from_millis(500);

/// Start every capture and hand back the processes, the packet channel, and
/// whatever the capture helpers complained about.
///
/// The order of attempts matters: unprivileged where that may be enough (Linux
/// capabilities), then `pkexec`/`sudo`. `ngrep` exits at once when it cannot
/// open the device, so a probe window decides whether to keep or escalate.
pub(crate) fn start_capture(
    interfaces: &[String],
    elevation: &Elevation,
    elevate: bool,
) -> io::Result<(Vec<Child>, Receiver<Packet>, Receiver<String>)> {
    let requested = env::var("NGREP").unwrap_or_else(|_| "ngrep".into());
    let binary = resolve_binary(&requested).ok_or_else(|| {
        io::Error::other(format!(
            "`{requested}` not found; install it (`brew install ngrep`, `apt install ngrep`, `pacman -S ngrep`) or point NGREP at it"
        ))
    })?;
    let mut attempts: Vec<bool> = Vec::new();
    if !elevate || matches!(elevation, Elevation::None) {
        attempts.push(false);
    } else {
        if try_unprivileged_first() {
            attempts.push(false);
        }
        attempts.push(true);
    }
    let mut failures = String::new();
    for elevated in attempts {
        let prefix = if elevated {
            elevation.argv_prefix()?
        } else {
            Vec::new()
        };
        let (tx, rx) = mpsc::channel();
        let (err_tx, err_rx) = mpsc::channel();
        let mut captures = Vec::new();
        let mut attempt_failure = String::new();
        for interface in interfaces {
            match spawn_ngrep(&prefix, &binary, interface, tx.clone(), err_tx.clone()) {
                Ok(child) => captures.push(child),
                Err(err) => attempt_failure.push_str(&format!("  {interface}: {err}\n")),
            }
        }
        drop(tx);
        drop(err_tx);
        if !captures.is_empty() {
            thread::sleep(CAPTURE_PROBE);
            let (running, detail) = reap(&mut captures, &err_rx);
            attempt_failure.push_str(&detail);
            if running > 0 {
                // Some legs may still be missing; say so while the terminal is
                // still a normal one instead of leaving a silently partial view.
                if !attempt_failure.is_empty() {
                    eprintln!("sniff-llms: partial capture:\n{attempt_failure}");
                }
                return Ok((captures, rx, err_rx));
            }
            for capture in captures.iter_mut() {
                let _ = capture.kill();
            }
            failures.push_str(&attempt_failure);
        }
    }
    Err(io::Error::other(format!(
        "could not capture on {}:\n{failures}  check the interface name (`ifconfig -l`, `ls /sys/class/net`) and the capture rights",
        interfaces.join(", ")
    )))
}

/// Collect the captures that have exited and return `(still running, what they
/// said)`. `ngrep` reports device errors on stderr and exits immediately, which
/// a full-screen UI would hide behind a quietly empty chat list.
pub(crate) fn reap(captures: &mut [Child], errors: &Receiver<String>) -> (usize, String) {
    let mut detail = String::new();
    while let Ok(line) = errors.try_recv() {
        detail.push_str("ngrep: ");
        detail.push_str(&line);
        detail.push('\n');
    }
    let mut running = 0;
    for child in captures.iter_mut() {
        if child.try_wait().ok().flatten().is_none() {
            running += 1;
        }
    }
    (running, detail)
}
