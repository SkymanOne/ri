//! The `--resume` session picker that runs before interactive mode starts.
//!
//! Port of `cli/session-picker.ts` in `packages/coding-agent/src` in pi
//! `v1.0.0`: the `/resume` selector on the main screen, without renaming.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ri_tui::color::ColorMode;
use ri_tui::input::{Input, InputBuffer, escape_timeout};
use ri_tui::keys::Keys;
use ri_tui::screen::MainScreen;
use ri_tui::terminal::{
    BRACKETED_PASTE_DISABLE, BRACKETED_PASTE_ENABLE, ColorQuery, Filtered, KeyboardProtocol,
    color_query,
};

use super::selectors::{Action, Outcome, Ui};
use super::session_selector::{SessionSelector, Sources};
use super::{COLOR_QUERY_TIMEOUT, emit, home_dir, keybindings, load_theme, true_color};

/// Shows the session selector; the chosen session file, or `None` when
/// cancelled.
pub fn pick_session(
    agent_dir: &Path,
    sources: Sources,
    theme_setting: Option<&str>,
) -> std::io::Result<Option<PathBuf>> {
    #[cfg(unix)]
    let raw = ri_tui::terminal::RawMode::enable()?;
    let mut protocol = KeyboardProtocol::default();
    let mut query = ColorQuery::new();
    emit(&format!(
        "{BRACKETED_PASTE_ENABLE}{}{}",
        protocol.query(),
        color_query()
    ));
    let mut buffer = InputBuffer::new();
    let mut keys_in: Vec<String> = Vec::new();
    let read = |buffer: &mut InputBuffer,
                protocol: &mut KeyboardProtocol,
                query: &mut ColorQuery,
                timeout: Duration|
     -> Vec<String> {
        let mut out = Vec::new();
        #[cfg(unix)]
        if !ri_tui::terminal::stdin_ready(timeout) {
            return out;
        }
        let mut bytes = [0u8; 4096];
        let count = std::io::Read::read(&mut std::io::stdin(), &mut bytes).unwrap_or(0);
        let mut write = String::new();
        for input in buffer.push(&bytes[..count]) {
            match input {
                Input::Key(sequence) => {
                    if let Filtered::Forward(keys) = protocol.filter(&sequence, &mut write) {
                        out.extend(keys.into_iter().filter(|key| !query.consume(key)));
                    }
                }
                Input::Paste(text) => out.push(format!("\x1b[200~{text}\x1b[201~")),
            }
        }
        if !write.is_empty() {
            emit(&write);
        }
        out
    };
    let deadline = Instant::now() + COLOR_QUERY_TIMEOUT;
    while !query.is_done() && Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        keys_in.extend(read(&mut buffer, &mut protocol, &mut query, remaining));
    }
    let mode = if true_color() {
        ColorMode::TrueColor
    } else {
        ColorMode::Ansi256
    };
    let (theme, _) = load_theme(theme_setting, agent_dir, &query.colors(), mode);
    let mut keys = keybindings::load(agent_dir, Keys::detect(protocol.kitty));
    keys.set_kitty(protocol.kitty);
    let home = home_dir().and_then(|home| home.to_str().map(str::to_owned));
    let mut selector = SessionSelector::new(sources, None, home, false);
    let mut screen = MainScreen::new();
    let escape_wait = escape_timeout(|name| std::env::var(name).ok());
    let mut chosen = None;
    'outer: loop {
        let ui = Ui {
            theme: &theme,
            keys: &keys,
        };
        let (width, height) = ri_tui::terminal::size();
        let (rows, cursor) = selector.render(width, &ui);
        emit(&screen.frame(&rows, cursor, width, height));
        for key in std::mem::take(&mut keys_in) {
            if ri_tui::keys::is_key_release(&key) {
                continue;
            }
            match selector.handle_input(&key, &ui) {
                Outcome::Done(Action::Resume(path)) => {
                    chosen = Some(path);
                    break 'outer;
                }
                Outcome::Cancel => break 'outer,
                _ => {}
            }
        }
        let wait = buffer
            .timeout(escape_wait)
            .unwrap_or(Duration::from_millis(250));
        keys_in = read(&mut buffer, &mut protocol, &mut query, wait);
        if keys_in.is_empty() {
            keys_in.extend(buffer.flush().into_iter().filter_map(|input| match input {
                Input::Key(key) => Some(key),
                Input::Paste(_) => None,
            }));
            keys_in.extend(protocol.flush());
        }
        keys.set_kitty(protocol.kitty);
    }
    let mut out = screen.stop();
    out.push_str(BRACKETED_PASTE_DISABLE);
    out.push_str(&protocol.disable());
    out.push_str("\x1b[?25h");
    emit(&out);
    #[cfg(unix)]
    raw.restore();
    Ok(chosen)
}
