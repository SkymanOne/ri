//! Copying text to the system clipboard.
//!
//! Port of `copyToClipboard` in `packages/coding-agent/src/utils/clipboard.ts`
//! in pi `v1.0.0`, without pi's native clipboard addon: platform commands
//! first, then OSC 52 for remote and headless sessions.

use base64::Engine as _;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const MAX_OSC52_ENCODED_LENGTH: usize = 100_000;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn is_wsl() -> bool {
    env("WSL_DISTRO_NAME").is_some()
        || std::fs::read_to_string("/proc/version")
            .is_ok_and(|version| version.to_lowercase().contains("microsoft"))
}

/// Runs `command` with `input` on stdin; whether it exited successfully in time.
fn run(command: &str, args: &[&str], input: &str) -> bool {
    let Ok(mut child) = Command::new(command)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input.as_bytes());
    }
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                return false;
            }
        }
    }
}

/// The OSC 52 sequence that sets the clipboard, or `None` when too long.
fn osc52(text: &str) -> Option<String> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    (encoded.len() <= MAX_OSC52_ENCODED_LENGTH).then(|| format!("\x1b]52;c;{encoded}\x07"))
}

/// Copies `text`. Terminal output (OSC 52) goes through `emit`.
pub fn copy(text: &str, emit: impl Fn(&str)) -> Result<(), String> {
    let linux = cfg!(target_os = "linux");
    let mut commands: Vec<(&str, Vec<&str>)> = Vec::new();
    if cfg!(target_os = "macos") {
        commands.push(("pbcopy", vec![]));
    } else if cfg!(windows) {
        commands.push(("clip", vec![]));
    } else {
        if env("TERMUX_VERSION").is_some() {
            commands.push(("termux-clipboard-set", vec![]));
        }
        if env("WAYLAND_DISPLAY").is_some() {
            commands.push(("wl-copy", vec![]));
        }
        if env("DISPLAY").is_some() {
            commands.push(("xclip", vec!["-selection", "clipboard"]));
            commands.push(("xsel", vec!["--clipboard", "--input"]));
        }
    }
    let mut copied = commands
        .iter()
        .any(|(command, args)| run(command, args, text));
    let mut emitted = false;
    if !copied
        && linux
        && is_wsl()
        && env("WT_SESSION").is_some()
        && let Some(sequence) = osc52(text)
    {
        emit(&sequence);
        emitted = true;
        copied = true;
    }
    let remote = ["SSH_CONNECTION", "SSH_CLIENT", "MOSH_CONNECTION"]
        .iter()
        .any(|name| env(name).is_some());
    let headless = linux
        && env("DISPLAY").is_none()
        && env("WAYLAND_DISPLAY").is_none()
        && env("TERMUX_VERSION").is_none();
    let mut oversized = false;
    if !emitted && (remote || (!copied && headless)) {
        match osc52(text) {
            Some(sequence) => {
                emit(&sequence);
                copied = true;
            }
            None => oversized = true,
        }
    }
    if copied {
        return Ok(());
    }
    if oversized {
        return Err("Clipboard unavailable: text exceeds the OSC 52 size limit".into());
    }
    if linux {
        if env("TERMUX_VERSION").is_some() {
            return Err(
                "Clipboard unavailable: install the Termux:API app and `termux-api` package".into(),
            );
        }
        if env("WAYLAND_DISPLAY").is_some() {
            return Err(
                "Clipboard unavailable: install `wl-clipboard` (`wl-copy`) or check Wayland access"
                    .into(),
            );
        }
        if env("DISPLAY").is_some() {
            return Err(
                "Clipboard unavailable: install `xclip` or `xsel`, or check X11 access".into(),
            );
        }
    }
    Err("Clipboard unavailable".into())
}
