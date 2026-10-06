//! Session export to a self-contained HTML viewer.
//!
//! Port of `core/export-html/index.ts` in `packages/coding-agent/src` in pi
//! `v1.0.0`. The template, its styles and script, and the marked and
//! highlight.js builds it embeds are pi's, unchanged (`assets/export-html`).

use std::path::{Path, PathBuf};

use base64::Engine;
use serde_json::{Map, Value};
use yapi_tui::color::Color;
use yapi_tui::theme::{Appearance, Theme};

const TEMPLATE: &str = include_str!("../assets/export-html/template.html");
const CSS: &str = include_str!("../assets/export-html/template.css");
const JS: &str = include_str!("../assets/export-html/template.js");
const MARKED: &str = include_str!("../assets/export-html/vendor/marked.min.js");
const HIGHLIGHT: &str = include_str!("../assets/export-html/vendor/highlight.min.js");

/// What the viewer shows: pi's `SessionData`.
pub struct SessionData {
    /// The session header as stored.
    pub header: Value,
    /// Every other entry as stored, in file order.
    pub entries: Vec<Value>,
    /// The entry the viewer opens at.
    pub leaf_id: Option<String>,
    /// The prompt of a live session.
    pub system_prompt: Option<String>,
    /// `{name, description, parameters}` of a live session's tools.
    pub tools: Option<Vec<Value>>,
}

impl SessionData {
    /// The data of a session manager's entries, without a live session's
    /// prompt and tools.
    pub fn of(session: &yapi_core::session::SessionManager) -> SessionData {
        let (header, entries) = session.documents();
        SessionData {
            header: header.cloned().unwrap_or(Value::Null),
            entries: entries.into_iter().cloned().collect(),
            leaf_id: session.leaf_id().map(str::to_owned),
            system_prompt: None,
            tools: None,
        }
    }

    /// `JSON.stringify(sessionData)`.
    fn to_json(&self) -> String {
        let mut object = Map::new();
        object.insert("header".into(), self.header.clone());
        object.insert("entries".into(), Value::Array(self.entries.clone()));
        object.insert(
            "leafId".into(),
            self.leaf_id.clone().map_or(Value::Null, Value::String),
        );
        if let Some(prompt) = &self.system_prompt {
            object.insert("systemPrompt".into(), Value::String(prompt.clone()));
        }
        if let Some(tools) = &self.tools {
            object.insert("tools".into(), Value::Array(tools.clone()));
        }
        yapi_types::json::stringify(&object)
    }
}

/// JavaScript's `String.prototype.replace` with a string pattern: the first
/// match only, with the replacement's `$$`, `$&`, `` $` `` and `$'` patterns.
/// pi fills its template this way, and the embedded scripts contain such
/// patterns, so the result depends on it.
fn js_replace(text: &str, pattern: &str, replacement: &str) -> String {
    let Some(at) = text.find(pattern) else {
        return text.to_owned();
    };
    let (before, after) = (&text[..at], &text[at + pattern.len()..]);
    let mut out = String::with_capacity(text.len() + replacement.len());
    out.push_str(before);
    let mut rest = replacement;
    while let Some(dollar) = rest.find('$') {
        out.push_str(&rest[..dollar]);
        let tail = &rest[dollar + 1..];
        match tail.chars().next() {
            Some('$') => out.push('$'),
            Some('&') => out.push_str(pattern),
            Some('`') => out.push_str(before),
            Some('\'') => out.push_str(after),
            _ => {
                out.push('$');
                rest = tail;
                continue;
            }
        }
        rest = &tail[1..];
    }
    out.push_str(rest);
    out.push_str(after);
    out
}

/// `#rrggbb` or `rgb(r, g, b)`, as pi's `parseColor`.
fn parse_css_color(color: &str) -> Option<[f64; 3]> {
    if let Some(hex) = color.strip_prefix('#')
        && hex.len() == 6
        && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        let channel = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok().map(f64::from);
        return Some([channel(0)?, channel(2)?, channel(4)?]);
    }
    let inner = color.strip_prefix("rgb")?.trim_start().strip_prefix('(')?;
    let inner = inner.strip_suffix(')')?;
    let mut channels = inner
        .split(',')
        .map(|part| part.trim().parse::<u8>().ok().map(f64::from));
    let rgb = [channels.next()??, channels.next()??, channels.next()??];
    channels.next().is_none().then_some(rgb)
}

fn luminance([r, g, b]: [f64; 3]) -> f64 {
    let linear = |c: f64| {
        let s = c / 255.0;
        if s <= 0.03928 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
}

/// JavaScript's `Math.round`, which rounds halves up.
fn adjust_brightness(color: [f64; 3], factor: f64) -> String {
    let [r, g, b] = color.map(|c| yapi_types::js::round(c * factor).clamp(0.0, 255.0));
    format!("rgb({r}, {g}, {b})")
}

/// pi's `deriveExportColors`: page, card and info backgrounds from a base.
fn derive_export_colors(base: &str) -> [String; 3] {
    let Some(rgb) = parse_css_color(base) else {
        return [
            "rgb(24, 24, 30)".into(),
            "rgb(30, 30, 36)".into(),
            "rgb(60, 55, 40)".into(),
        ];
    };
    let [r, g, b] = rgb;
    if luminance(rgb) > 0.5 {
        [
            adjust_brightness(rgb, 0.96),
            base.to_owned(),
            format!(
                "rgb({}, {}, {})",
                (r + 10.0).min(255.0),
                (g + 5.0).min(255.0),
                (b - 20.0).max(0.0)
            ),
        ]
    } else {
        [
            adjust_brightness(rgb, 0.7),
            adjust_brightness(rgb, 0.85),
            format!(
                "rgb({}, {}, {b})",
                (r + 20.0).min(255.0),
                (g + 15.0).min(255.0)
            ),
        ]
    }
}

/// The theme the export is drawn with, and the terminal's default colors
/// that fill its default tokens when known.
pub struct ExportTheme<'a> {
    /// The theme.
    pub theme: &'a Theme,
    /// The terminal's default foreground.
    pub foreground: Option<Color>,
    /// The terminal's default background.
    pub background: Option<Color>,
    /// The terminal's appearance, for themes without one of their own.
    pub appearance: Appearance,
}

/// pi's `generateHtml`.
pub fn render(data: &SessionData, theme: &ExportTheme<'_>) -> String {
    let colors: Vec<(String, String)> = theme
        .theme
        .resolved_colors(theme.foreground, theme.background, theme.appearance)
        .into_iter()
        .map(|(token, color)| (token, color.to_hex()))
        .collect();
    let user_message_bg = colors
        .iter()
        .find(|(token, _)| token == "userMessageBg")
        .map(|(_, color)| color.as_str())
        .filter(|color| !color.is_empty())
        .unwrap_or("#343541");
    let derived = derive_export_colors(user_message_bg);
    let declared = theme.theme.export_colors();
    let export: Vec<String> = (0..3)
        .map(|index| {
            declared[index]
                .map(str::to_owned)
                .unwrap_or_else(|| derived[index].clone())
        })
        .collect();
    let mut vars: Vec<String> = colors
        .iter()
        .map(|(token, color)| format!("--{token}: {color};"))
        .collect();
    vars.push(format!("--exportPageBg: {};", export[0]));
    vars.push(format!("--exportCardBg: {};", export[1]));
    vars.push(format!("--exportInfoBg: {};", export[2]));
    let data = base64::engine::general_purpose::STANDARD.encode(data.to_json());
    let css = js_replace(CSS, "{{THEME_VARS}}", &vars.join("\n      "));
    let css = js_replace(&css, "{{BODY_BG}}", &export[0]);
    let css = js_replace(&css, "{{CONTAINER_BG}}", &export[1]);
    let css = js_replace(&css, "{{INFO_BG}}", &export[2]);
    let html = js_replace(TEMPLATE, "{{CSS}}", &css);
    let html = js_replace(&html, "{{JS}}", JS);
    let html = js_replace(&html, "{{SESSION_DATA}}", &data);
    let html = js_replace(&html, "{{MARKED_JS}}", MARKED);
    js_replace(&html, "{{HIGHLIGHT_JS}}", HIGHLIGHT)
}

/// Where an export goes: `output`, else `yapi-session-<name>.html` in the
/// working directory, as pi's default.
pub fn output_path(output: Option<&str>, session_file: &Path) -> PathBuf {
    match output {
        Some(path) => PathBuf::from(yapi_core::tools::path::expand(path)),
        None => {
            let name = session_file
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default();
            PathBuf::from(format!("yapi-session-{name}.html"))
        }
    }
}

/// pi's `exportFromFile`: a session file to HTML with the system theme, as
/// `--export` writes it. Returns where it wrote.
pub fn export_file(input: &str, output: Option<&str>) -> Result<PathBuf, String> {
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    let source = yapi_core::tools::path::resolve_to_cwd(input, &cwd);
    if !source.exists() {
        return Err(format!("File not found: {}", source.display()));
    }
    let session = yapi_core::session::SessionManager::open(&source, None, None)
        .map_err(|error| error.to_string())?;
    let data = SessionData::of(&session);
    let appearance = yapi_tui::theme::detect_colorfgbg(std::env::var("COLORFGBG").ok().as_deref())
        .unwrap_or(Appearance::Dark);
    let theme = system_theme(appearance);
    let html = render(
        &data,
        &ExportTheme {
            theme: &theme,
            foreground: None,
            background: None,
            appearance,
        },
    );
    let target = output_path(output, &source);
    std::fs::write(&target, html).map_err(|error| error.to_string())?;
    Ok(target)
}

/// pi's `exportSessionToHtml`: the live session, with its prompt and tools.
/// Returns where it wrote.
pub fn export_session(
    session: &yapi_core::agent_session::AgentSession,
    output: Option<&str>,
    theme: &ExportTheme<'_>,
) -> Result<PathBuf, String> {
    let (file, mut data) = session.with_session(|manager| {
        (
            manager.file().map(Path::to_path_buf),
            SessionData::of(manager),
        )
    });
    let Some(file) = file else {
        return Err("Cannot export in-memory session to HTML".into());
    };
    if !file.exists() {
        return Err("Nothing to export yet - start a conversation first".into());
    }
    let (system_prompt, tools) = session.export_context();
    data.system_prompt = system_prompt;
    data.tools = Some(tools);
    let target = output_path(output, &file);
    std::fs::write(&target, render(&data, theme))
        .map_err(|error| yapi_core::tools::node_error(&error, "open", &target))?;
    Ok(target)
}

/// pi's system theme where the terminal reported no colors.
pub fn system_theme(appearance: Appearance) -> Theme {
    Theme::system(
        &yapi_tui::theme::SystemThemeInput {
            foreground: None,
            background: None,
            palette: None,
            saturation: 1.0,
            appearance_hint: Some(appearance),
        },
        yapi_tui::color::ColorMode::TrueColor,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacement_patterns_follow_javascript() {
        assert_eq!(js_replace("a{{X}}b{{X}}", "{{X}}", "1"), "a1b{{X}}");
        assert_eq!(
            js_replace("a{{X}}b", "{{X}}", "$$-$&-$`-$'-$1-$"),
            "a$-{{X}}-a-b-$1-$b"
        );
        assert_eq!(js_replace("ab", "{{X}}", "1"), "ab");
    }

    #[test]
    fn export_colors_derive_from_the_user_message_background() {
        assert_eq!(
            derive_export_colors("#343541"),
            ["rgb(36, 37, 46)", "rgb(44, 45, 55)", "rgb(72, 68, 65)"]
        );
        assert_eq!(
            derive_export_colors("#f0f0f0"),
            ["rgb(230, 230, 230)", "#f0f0f0", "rgb(250, 245, 220)"]
        );
        assert_eq!(derive_export_colors("oops")[0], "rgb(24, 24, 30)");
    }
}
