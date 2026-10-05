//! Selectors that run outside an interactive session: the `--resume` session
//! picker, the project trust prompt and `yapi config`.
//!
//! Ports of `cli/session-picker.ts`, the startup selector of
//! `cli/startup-ui.ts` and `cli/config-selector.ts` in
//! `packages/coding-agent/src` in pi `v1.0.0`, on the main screen.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use yapi_tui::color::ColorMode;
use yapi_tui::input::{Input, InputBuffer, escape_timeout};
use yapi_tui::keys::Keys;
use yapi_tui::screen::MainScreen;
use yapi_tui::terminal::{
    BRACKETED_PASTE_DISABLE, BRACKETED_PASTE_ENABLE, ColorQuery, Filtered, KeyboardProtocol,
    color_query,
};

use super::config_selector::ConfigSelector;
use super::selectors::{Action, ChoiceDialog, Outcome, Selector, Ui};
use super::session_selector::{SessionSelector, Sources};
use super::{COLOR_QUERY_TIMEOUT, emit, home_dir, keybindings, load_theme, true_color};

/// Shows the session selector; the chosen session file, or `None` when
/// cancelled.
pub fn pick_session(
    agent_dir: &Path,
    sources: Sources,
    theme_setting: Option<&str>,
) -> std::io::Result<Option<PathBuf>> {
    let home = home_dir().and_then(|home| home.to_str().map(str::to_owned));
    let selector = Selector::Session(Box::new(SessionSelector::new(sources, None, home, false)));
    Ok(
        match run_selector(agent_dir, theme_setting, selector, false)? {
            Some(Action::Resume(path)) => Some(path),
            _ => None,
        },
    )
}

/// pi's startup trust prompt for `cwd`: stores the chosen decision and returns
/// whether the project is trusted; `None` when cancelled.
pub fn ask_project_trust(
    agent_dir: &Path,
    cwd: &Path,
    theme_setting: Option<&str>,
) -> std::io::Result<Option<bool>> {
    let options = yapi_core::trust::trust_options(cwd, true);
    let labels: Vec<&str> = options.iter().map(|option| option.label.as_str()).collect();
    let title = yapi_core::trust::prompt_title(cwd);
    let Some(index) = ask_choice(agent_dir, theme_setting, &title, &labels)? else {
        return Ok(None);
    };
    let Some(option) = options.get(index) else {
        return Ok(None);
    };
    if !option.updates.is_empty() {
        yapi_core::trust::TrustStore::new(agent_dir).set_many(&option.updates)?;
    }
    Ok(Some(option.trusted))
}

/// pi's `showStartupSelector`: `title` over `labels` before the interactive
/// mode starts; the chosen index, or `None` when cancelled.
pub fn ask_choice(
    agent_dir: &Path,
    theme_setting: Option<&str>,
    title: &str,
    labels: &[&str],
) -> std::io::Result<Option<usize>> {
    let dialog = Selector::Choice(ChoiceDialog::new(title, labels));
    Ok(
        match run_selector(agent_dir, theme_setting, dialog, true)? {
            Some(Action::Choice(index)) => Some(index),
            _ => None,
        },
    )
}

/// Shows the `yapi config` selector until it is closed.
pub fn configure(
    agent_dir: &Path,
    theme_setting: Option<&str>,
    selector: ConfigSelector,
) -> std::io::Result<()> {
    run_selector(
        agent_dir,
        theme_setting,
        Selector::Config(Box::new(selector)),
        false,
    )?;
    Ok(())
}

/// Runs `selector` on the main screen until it finishes; its action, or
/// `None` when cancelled. With `clear`, its rows are erased afterwards.
fn run_selector(
    agent_dir: &Path,
    theme_setting: Option<&str>,
    mut selector: Selector,
    clear: bool,
) -> std::io::Result<Option<Action>> {
    #[cfg(unix)]
    let raw = yapi_tui::terminal::RawMode::enable()?;
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
        if !yapi_tui::terminal::stdin_ready(timeout) {
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
    let (theme, _) = load_theme(
        theme_setting,
        &super::themes::ThemeFiles::default(),
        agent_dir,
        &query.colors(),
        mode,
    );
    let mut keys = keybindings::load(agent_dir, Keys::detect(protocol.kitty));
    keys.set_kitty(protocol.kitty);
    let mut screen = MainScreen::new();
    let escape_wait = escape_timeout(|name| std::env::var(name).ok());
    let mut chosen = None;
    'outer: loop {
        let ui = Ui {
            theme: &theme,
            keys: &keys,
        };
        let (width, height) = yapi_tui::terminal::size();
        let (rows, cursor) = selector.render(width, &ui);
        emit(&screen.frame(&rows, cursor, width, height));
        for key in std::mem::take(&mut keys_in) {
            if yapi_tui::keys::is_key_release(&key) {
                continue;
            }
            match selector.handle_input(&key, &ui) {
                Outcome::Done(action) => {
                    chosen = Some(action);
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
    if clear {
        let (width, height) = yapi_tui::terminal::size();
        emit(&screen.frame(&[], None, width, height));
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
