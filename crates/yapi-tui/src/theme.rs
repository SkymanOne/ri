//! Themes: named color tokens resolved to terminal styles.
//!
//! Port of `packages/coding-agent/src/modes/interactive/theme/theme.ts` and
//! `system-theme.ts` in pi `v1.0.0`. A theme is either a JSON document (the
//! built-in `dark` and `light`, or a user's file) or the `system` theme, which
//! is generated from the colors the terminal reports.

use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;
use ratatui_core::style::{Modifier, Style};
use serde_json::{Map, Value};

use crate::color::{
    Color, ColorMode, InvalidColor, bisect, okhsl_to_rgb, oklab_to_okhsl_lightness,
};

/// The theme pi uses when none is configured.
pub const SYSTEM_THEME_NAME: &str = "system";

/// The built-in `dark` theme document.
pub const DARK_THEME: &str = include_str!("../themes/dark.json");
/// The built-in `light` theme document.
pub(crate) const LIGHT_THEME: &str = include_str!("../themes/light.json");

/// Background tokens; every other token is a foreground.
pub const BACKGROUND_TOKENS: [&str; 7] = [
    "selectedBg",
    "searchMatchBg",
    "userMessageBg",
    "customMessageBg",
    "toolPendingBg",
    "toolSuccessBg",
    "toolErrorBg",
];

/// Tokens a theme document must define.
pub(crate) const REQUIRED_TOKENS: [&str; 51] = [
    "accent",
    "border",
    "borderAccent",
    "borderMuted",
    "success",
    "error",
    "warning",
    "muted",
    "dim",
    "text",
    "thinkingText",
    "selectedBg",
    "userMessageBg",
    "userMessageText",
    "customMessageBg",
    "customMessageText",
    "customMessageLabel",
    "toolPendingBg",
    "toolSuccessBg",
    "toolErrorBg",
    "toolTitle",
    "toolOutput",
    "mdHeading",
    "mdLink",
    "mdLinkUrl",
    "mdCode",
    "mdCodeBlock",
    "mdCodeBlockBorder",
    "mdQuote",
    "mdQuoteBorder",
    "mdHr",
    "mdListBullet",
    "toolDiffAdded",
    "toolDiffRemoved",
    "toolDiffContext",
    "syntaxComment",
    "syntaxKeyword",
    "syntaxFunction",
    "syntaxVariable",
    "syntaxString",
    "syntaxNumber",
    "syntaxType",
    "syntaxOperator",
    "syntaxPunctuation",
    "thinkingOff",
    "thinkingMinimal",
    "thinkingLow",
    "thinkingMedium",
    "thinkingHigh",
    "thinkingXhigh",
    "bashMode",
];

/// Optional tokens and the token each falls back to.
const OPTIONAL_TOKENS: [(&str, &str); 5] = [
    ("scrollbarTrack", "muted"),
    ("scrollbarThumb", "text"),
    ("thinkingMax", "thinkingXhigh"),
    ("searchMatchBg", "selectedBg"),
    ("searchMatchText", "text"),
];

/// The background a theme is designed for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Appearance {
    /// Light text on a dark background.
    Dark,
    /// Dark text on a light background.
    Light,
}

impl Appearance {
    fn parse(value: &str) -> Option<Appearance> {
        match value {
            "dark" => Some(Appearance::Dark),
            "light" => Some(Appearance::Light),
            _ => None,
        }
    }
}

/// A token's paint: the terminal's default color, or a color.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Paint {
    /// The terminal's default foreground or background.
    Default,
    /// A concrete color.
    Color(Color),
}

/// Errors loading a theme document.
#[derive(Debug, thiserror::Error)]
pub enum ThemeError {
    /// The document is not JSON.
    #[error("Failed to parse theme {label}: {message}")]
    Parse {
        /// File path or theme name.
        label: String,
        /// The parser's message.
        message: String,
    },
    /// Required color tokens are missing; pi's schema message.
    #[error(
        "Invalid theme \"{label}\":\n\nMissing required color tokens:\n{}\n\nPlease add these colors to your theme's \"colors\" object.\nSee the built-in themes (dark.json, light.json) for reference values.",
        tokens.iter().map(|token| format!("  - {token}")).collect::<Vec<_>>().join("\n")
    )]
    MissingColors {
        /// File path or theme name.
        label: String,
        /// The missing tokens, sorted.
        tokens: Vec<String>,
    },
    /// The document's shape is wrong.
    #[error("Invalid theme \"{label}\": {message}")]
    Invalid {
        /// File path or theme name.
        label: String,
        /// What is wrong.
        message: String,
    },
    /// A variable reference loops.
    #[error("Circular variable reference detected: {0}")]
    Circular(String),
    /// A value names a variable that does not exist.
    #[error("Variable reference not found: {0}")]
    MissingVariable(String),
    /// A value is not a color.
    #[error(transparent)]
    Color(#[from] InvalidColor),
}

/// A resolved theme.
#[derive(Clone, Debug)]
pub struct Theme {
    /// The theme's name.
    pub name: Option<String>,
    mode: ColorMode,
    /// Paints in pi's token order: the document's, then fallbacks it lacks.
    paints: IndexMap<String, Paint>,
    dim: HashSet<String>,
    appearance: Option<Appearance>,
    /// The `export` section's page, card and info backgrounds.
    export: [Option<String>; 3],
}

/// The terminal's default colors when it did not report them, by appearance:
/// pi's `GUESSED_DEFAULT_COLORS`.
fn guessed_defaults(appearance: Appearance) -> (Color, Color) {
    match appearance {
        Appearance::Dark => (Color::Rgb(229.0, 229.0, 231.0), Color::Rgb(0.0, 0.0, 0.0)),
        Appearance::Light => (Color::Rgb(0.0, 0.0, 0.0), Color::Rgb(255.0, 255.0, 255.0)),
    }
}

/// An `export` color for CSS, as pi's `getThemeExportColors` resolves it:
/// palette indexes and `okhsl()` become hex, `""` means unset, and other
/// values pass through.
fn export_color(value: &Value, vars: &Map<String, Value>) -> Option<String> {
    let resolved = resolve(value, vars, &mut Vec::new()).ok()?;
    match resolved {
        Value::Number(number) => {
            let index = u8::try_from(number.as_u64()?).ok()?;
            Some(Color::Indexed(index).to_hex())
        }
        Value::String(text) if text.is_empty() => None,
        Value::String(text) if text.to_ascii_lowercase().starts_with("okhsl(") => {
            Color::parse(&text).ok().map(Color::to_hex)
        }
        Value::String(text) => Some(text),
        _ => None,
    }
}

fn is_literal(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    value.is_empty()
        || value.starts_with('#')
        || lower.starts_with("oklch(")
        || lower.starts_with("okhsl(")
}

fn resolve(
    value: &Value,
    vars: &Map<String, Value>,
    visited: &mut Vec<String>,
) -> Result<Value, ThemeError> {
    let Some(text) = value.as_str() else {
        return Ok(value.clone());
    };
    if is_literal(text) {
        return Ok(value.clone());
    }
    if visited.iter().any(|seen| seen == text) {
        return Err(ThemeError::Circular(text.to_owned()));
    }
    let Some(next) = vars.get(text) else {
        return Err(ThemeError::MissingVariable(text.to_owned()));
    };
    visited.push(text.to_owned());
    resolve(next, vars, visited)
}

fn paint_of(value: &Value) -> Result<Paint, ThemeError> {
    match value {
        Value::String(text) if text.is_empty() => Ok(Paint::Default),
        Value::String(text) => Ok(Paint::Color(Color::parse(text)?)),
        Value::Number(number) => number
            .as_u64()
            .filter(|index| *index <= 255)
            .map(|index| Paint::Color(Color::Indexed(index as u8)))
            .ok_or_else(|| InvalidColor(number.to_string()).into()),
        other => Err(InvalidColor(other.to_string()).into()),
    }
}

fn average_lightness(colors: &[Color]) -> Option<f64> {
    let fixed: Vec<&Color> = colors
        .iter()
        .filter(|color| !matches!(color, Color::Indexed(index) if *index < 16))
        .collect();
    if fixed.is_empty() {
        return None;
    }
    Some(fixed.iter().map(|color| color.to_oklch().0).sum::<f64>() / fixed.len() as f64)
}

fn detect_appearance(foregrounds: &[Color], backgrounds: &[Color]) -> Option<Appearance> {
    let pick = |dark: bool| {
        if dark {
            Appearance::Dark
        } else {
            Appearance::Light
        }
    };
    match (
        average_lightness(foregrounds),
        average_lightness(backgrounds),
    ) {
        (Some(fg), Some(bg)) => Some(pick(bg < fg)),
        (None, Some(bg)) => Some(pick(bg < 0.5)),
        (Some(fg), None) => Some(pick(fg > 0.5)),
        (None, None) => None,
    }
}

impl Theme {
    fn new(
        name: Option<String>,
        paints: IndexMap<String, Paint>,
        dim: HashSet<String>,
        appearance: Option<Appearance>,
        mode: ColorMode,
    ) -> Theme {
        let appearance = appearance.or_else(|| {
            let (mut fg, mut bg) = (Vec::new(), Vec::new());
            for (token, paint) in &paints {
                if let Paint::Color(color) = paint {
                    if BACKGROUND_TOKENS.contains(&token.as_str()) {
                        bg.push(*color);
                    } else {
                        fg.push(*color);
                    }
                }
            }
            detect_appearance(&fg, &bg)
        });
        Theme {
            name,
            mode,
            paints,
            dim,
            appearance,
            export: [None, None, None],
        }
    }

    /// Parses and validates a theme document as pi loads the active theme:
    /// missing color tokens are reported before anything else. `label`
    /// names it in errors.
    pub fn from_json(label: &str, text: &str, mode: ColorMode) -> Result<Theme, ThemeError> {
        Theme::parse(label, text, mode, true)
    }

    /// Parses a theme document as pi registers theme files: colors are
    /// resolved before missing tokens are reported.
    pub fn from_json_lenient(
        label: &str,
        text: &str,
        mode: ColorMode,
    ) -> Result<Theme, ThemeError> {
        Theme::parse(label, text, mode, false)
    }

    fn parse(
        label: &str,
        text: &str,
        mode: ColorMode,
        tokens_first: bool,
    ) -> Result<Theme, ThemeError> {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let json: Value = serde_json::from_str(text).map_err(|error| ThemeError::Parse {
            label: label.to_owned(),
            message: error.to_string(),
        })?;
        let invalid = |message: String| ThemeError::Invalid {
            label: label.to_owned(),
            message,
        };
        let colors = json
            .get("colors")
            .and_then(Value::as_object)
            .ok_or_else(|| invalid("expected an object with a \"colors\" map.".into()))?;
        let check_tokens = || {
            let mut missing: Vec<String> = REQUIRED_TOKENS
                .iter()
                .filter(|token| !colors.contains_key(**token))
                .map(|token| (*token).to_owned())
                .collect();
            missing.sort();
            if missing.is_empty() {
                Ok(())
            } else {
                Err(ThemeError::MissingColors {
                    label: label.to_owned(),
                    tokens: missing,
                })
            }
        };
        if tokens_first {
            check_tokens()?;
        }
        let empty = Map::new();
        let vars = json
            .get("vars")
            .and_then(Value::as_object)
            .unwrap_or(&empty);
        let mut paints = IndexMap::new();
        for (token, value) in colors {
            let resolved = resolve(value, vars, &mut Vec::new())?;
            paints.insert(token.clone(), paint_of(&resolved)?);
        }
        check_tokens()?;
        for (token, fallback) in OPTIONAL_TOKENS {
            if !paints.contains_key(token) {
                let paint = paints.get(fallback).copied().unwrap_or(Paint::Default);
                paints.insert(token.to_owned(), paint);
            }
        }
        let appearance = json
            .get("appearance")
            .and_then(Value::as_str)
            .and_then(Appearance::parse);
        let name = json.get("name").and_then(Value::as_str).map(str::to_owned);
        let mut theme = Theme::new(name, paints, HashSet::new(), appearance, mode);
        if let Some(export) = json.get("export").and_then(Value::as_object) {
            theme.export = ["pageBg", "cardBg", "infoBg"]
                .map(|key| export.get(key).and_then(|value| export_color(value, vars)));
        }
        Ok(theme)
    }

    /// A built-in theme by name: `dark` or `light`.
    pub fn builtin(name: &str, mode: ColorMode) -> Option<Theme> {
        let text = match name {
            "dark" => DARK_THEME,
            "light" => LIGHT_THEME,
            _ => return None,
        };
        Theme::from_json(name, text, mode).ok()
    }

    /// The system theme for what the terminal reported.
    pub fn system(input: &SystemThemeInput, mode: ColorMode) -> Theme {
        let generated = generate_system_theme(input);
        let paints = generated
            .colors
            .into_iter()
            .map(|(token, value)| {
                let paint = match value {
                    SystemValue::Default => Paint::Default,
                    SystemValue::Index(index) => Paint::Color(Color::Indexed(index)),
                    SystemValue::Rgb(rgb) => Paint::Color(Color::Rgb(rgb[0], rgb[1], rgb[2])),
                };
                (token.to_owned(), paint)
            })
            .collect();
        let dim = generated.dim.into_iter().map(str::to_owned).collect();
        Theme::new(
            Some(SYSTEM_THEME_NAME.to_owned()),
            paints,
            dim,
            generated.appearance,
            mode,
        )
    }

    /// The color mode styles are produced for.
    pub fn mode(&self) -> ColorMode {
        self.mode
    }

    /// The `export` section's page, card and info backgrounds as CSS colors,
    /// each `None` when unset.
    pub fn export_colors(&self) -> [Option<&str>; 3] {
        [0, 1, 2].map(|index| self.export[index].as_deref())
    }

    /// pi's `Theme.colors`: every token's concrete color, in pi's order.
    /// Tokens set to the terminal default take `foreground` or `background`,
    /// guessed from the appearance (else `fallback`) when unknown; faint
    /// tokens mix 40% toward the background.
    pub fn resolved_colors(
        &self,
        foreground: Option<Color>,
        background: Option<Color>,
        fallback: Appearance,
    ) -> Vec<(String, Color)> {
        let (guess_fg, guess_bg) = guessed_defaults(self.appearance.unwrap_or(fallback));
        let foreground = foreground.unwrap_or(guess_fg);
        let background = background.unwrap_or(guess_bg);
        let is_background = |token: &str| BACKGROUND_TOKENS.contains(&token);
        let side = |backgrounds: bool| {
            self.paints
                .keys()
                .filter(move |token| is_background(token) == backgrounds)
        };
        let mut concrete = Vec::new();
        let mut defaults = (Vec::new(), Vec::new());
        for token in side(false).chain(side(true)) {
            match self.paints.get(token) {
                Some(Paint::Color(color)) => concrete.push((token.clone(), *color)),
                _ if is_background(token) => defaults.1.push((token.clone(), background)),
                _ => defaults.0.push((token.clone(), foreground)),
            }
        }
        concrete.extend(defaults.0);
        concrete.extend(defaults.1);
        for (token, color) in &mut concrete {
            if self.dim.contains(token.as_str()) {
                *color = color.mix(background, 0.4);
            }
        }
        concrete
    }

    /// A token's paint.
    pub fn paint(&self, token: &str) -> Option<Paint> {
        self.paints.get(token).copied()
    }

    /// The names of its tokens.
    pub fn tokens(&self) -> impl Iterator<Item = &str> {
        self.paints.keys().map(String::as_str)
    }

    /// Whether foreground `token` is drawn faint.
    pub fn is_dim(&self, token: &str) -> bool {
        self.dim.contains(token)
    }

    /// The terminal color of `token`; the default color for tokens without
    /// one.
    pub fn color(&self, token: &str) -> ratatui_core::style::Color {
        match self.paints.get(token) {
            Some(Paint::Color(color)) => color.to_terminal(self.mode),
            _ => ratatui_core::style::Color::Reset,
        }
    }

    /// The style of foreground `token`; faint tokens also carry the dim modifier.
    pub fn fg(&self, token: &str) -> Style {
        let style = Style::new().fg(self.color(token));
        if self.dim.contains(token) {
            style.add_modifier(Modifier::DIM)
        } else {
            style
        }
    }

    /// The style of background `token`.
    pub fn bg(&self, token: &str) -> Style {
        Style::new().bg(self.color(token))
    }

    /// The editor border color for a thinking level.
    pub fn thinking_border(&self, level: &str) -> Style {
        let token = THINKING
            .into_iter()
            .find(|token| token["thinking".len()..].to_ascii_lowercase() == level);
        self.fg(token.unwrap_or("thinkingOff"))
    }
}

/// `light/dark` theme settings: the light and dark theme names.
pub(crate) fn parse_auto_theme_setting(setting: &str) -> Option<(String, String)> {
    let (light, dark) = setting.split_once('/')?;
    if dark.contains('/') {
        return None;
    }
    let (light, dark) = (light.trim(), dark.trim());
    (!light.is_empty() && !dark.is_empty()).then(|| (light.to_owned(), dark.to_owned()))
}

/// The theme name a setting selects on a terminal of `appearance`.
pub fn resolve_theme_setting(setting: Option<&str>, appearance: Appearance) -> Option<String> {
    let setting = setting?;
    if let Some((light, dark)) = parse_auto_theme_setting(setting) {
        return Some(if appearance == Appearance::Light {
            light
        } else {
            dark
        });
    }
    (!setting.contains('/')).then(|| setting.to_owned())
}

/// Dark or light from `COLORFGBG`, when its background is a palette index.
pub fn detect_colorfgbg(value: Option<&str>) -> Option<Appearance> {
    let bg = value?.split(';').next_back()?.trim();
    if bg.is_empty() || bg.len() > 2 || !bg.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let index: u8 = bg.parse().ok()?;
    (index <= 15).then_some(if index <= 6 || index == 8 {
        Appearance::Dark
    } else {
        Appearance::Light
    })
}

// System theme generation.

struct Family {
    hue: f64,
    min: f64,
    max: f64,
    slot: usize,
}

const fn family(hue: f64, min: f64, max: f64, slot: usize) -> Family {
    Family {
        hue,
        min,
        max,
        slot,
    }
}

const NEUTRAL: Family = family(231.49, 0.02, 0.08, 8);
const BLUE: Family = family(231.49, 0.1, 0.68, 4);
const GREEN: Family = family(158.68, 0.1, 0.76, 2);
const RED: Family = family(20.0, 0.1, 0.92, 1);
const YELLOW: Family = family(82.36, 0.5, 1.0, 3);
const ORANGE: Family = family(52.0, 0.12, 0.85, 3);
const VIOLET: Family = family(295.0, 0.2, 0.6, 5);
const CALAMINE: Family = family(202.43, 0.1, 0.74, 6);
const THINKING_SLATE: Family = family(231.49, 0.08, 0.2, 4);
const THINKING_BLUE: Family = family(231.49, 0.2, 0.45, 4);
const THINKING_PERIWINKLE: Family = family(263.25, 0.3, 0.6, 6);
const THINKING_VIOLET: Family = family(295.0, 0.4, 0.75, 5);
const THINKING_MAGENTA: Family = family(337.5, 0.5, 0.85, 13);
const THINKING_RED: Family = family(20.0, 0.95, 1.0, 1);

/// Every token with its color family, in pi's order.
const TOKEN_FAMILIES: [(&str, &Family); 56] = [
    ("selectedBg", &BLUE),
    ("searchMatchBg", &ORANGE),
    ("userMessageBg", &BLUE),
    ("customMessageBg", &VIOLET),
    ("toolPendingBg", &NEUTRAL),
    ("toolSuccessBg", &GREEN),
    ("toolErrorBg", &RED),
    ("text", &NEUTRAL),
    ("userMessageText", &NEUTRAL),
    ("customMessageText", &NEUTRAL),
    ("toolTitle", &NEUTRAL),
    ("syntaxOperator", &NEUTRAL),
    ("syntaxPunctuation", &NEUTRAL),
    ("muted", &NEUTRAL),
    ("dim", &NEUTRAL),
    ("thinkingText", &NEUTRAL),
    ("toolOutput", &NEUTRAL),
    ("mdLinkUrl", &NEUTRAL),
    ("mdQuote", &NEUTRAL),
    ("mdQuoteBorder", &NEUTRAL),
    ("mdHr", &NEUTRAL),
    ("mdCodeBlockBorder", &NEUTRAL),
    ("toolDiffContext", &NEUTRAL),
    ("syntaxComment", &NEUTRAL),
    ("scrollbarTrack", &NEUTRAL),
    ("scrollbarThumb", &NEUTRAL),
    ("searchMatchText", &NEUTRAL),
    ("borderMuted", &NEUTRAL),
    ("accent", &VIOLET),
    ("borderAccent", &VIOLET),
    ("customMessageLabel", &VIOLET),
    ("mdCode", &VIOLET),
    ("mdListBullet", &VIOLET),
    ("syntaxType", &VIOLET),
    ("border", &BLUE),
    ("mdLink", &BLUE),
    ("syntaxKeyword", &BLUE),
    ("syntaxVariable", &CALAMINE),
    ("success", &GREEN),
    ("mdCodeBlock", &GREEN),
    ("toolDiffAdded", &GREEN),
    ("bashMode", &GREEN),
    ("syntaxNumber", &GREEN),
    ("error", &RED),
    ("toolDiffRemoved", &RED),
    ("warning", &YELLOW),
    ("mdHeading", &YELLOW),
    ("syntaxFunction", &YELLOW),
    ("syntaxString", &ORANGE),
    ("thinkingOff", &NEUTRAL),
    ("thinkingMinimal", &THINKING_SLATE),
    ("thinkingLow", &THINKING_BLUE),
    ("thinkingMedium", &THINKING_PERIWINKLE),
    ("thinkingHigh", &THINKING_VIOLET),
    ("thinkingXhigh", &THINKING_MAGENTA),
    ("thinkingMax", &THINKING_RED),
];

fn token_family(token: &str) -> &'static Family {
    TOKEN_FAMILIES
        .iter()
        .find(|(name, _)| *name == token)
        .map_or(&NEUTRAL, |(_, family)| family)
}

fn token_slot(token: &str) -> usize {
    match token {
        "syntaxString" => 2,
        "syntaxNumber" => 5,
        "searchMatchBg" => 3,
        _ => token_family(token).slot,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Level {
    Panel,
    Track,
    Thinking(u8),
    Subtle,
    Thumb,
    Readable,
    Emphasis,
    TextOnPanel,
    Text,
}

struct Curve {
    coefficients: [f64; 6],
    reachable: (f64, f64),
}

const fn curve(coefficients: [f64; 6], low: f64, high: f64) -> Curve {
    Curve {
        coefficients,
        reachable: (low, high),
    }
}

fn level_curve(level: Level, appearance: Appearance) -> Curve {
    let dark = appearance == Appearance::Dark;
    match (level, dark) {
        (Level::Panel, true) => curve(
            [0.29131, -0.39746, 2.33185, -0.85524, -1.2076, 0.86276],
            0.0,
            0.979,
        ),
        (Level::Panel, false) => curve(
            [-3.74073, 27.94549, -78.44258, 112.6798, -79.60015, 22.11277],
            0.348,
            1.0,
        ),
        (Level::Track, true) => curve(
            [0.39028, -0.23015, 0.83573, 2.43829, -4.38292, 2.01582],
            0.0,
            0.946,
        ),
        (Level::Track, false) => curve(
            [
                -5.24921, 38.37322, -107.28833, 152.10005, -106.17127, 29.18061,
            ],
            0.368,
            1.0,
        ),
        (Level::Thinking(0), true) => curve(
            [0.52988, -0.05809, -0.30924, 4.63567, -6.52933, 2.89108],
            0.0,
            0.873,
        ),
        (Level::Thinking(0), false) => curve(
            [
                -28.27749, 182.85284, -469.62416, 603.15916, -384.59976, 97.35147,
            ],
            0.51,
            1.0,
        ),
        (Level::Thinking(1), true) => curve(
            [0.55278, -0.03667, -0.45659, 4.95347, -6.90265, 3.0706],
            0.0,
            0.858,
        ),
        (Level::Thinking(1), false) => curve(
            [
                -37.10484, 235.86282, -596.62344, 754.3633, -474.00763, 118.3551,
            ],
            0.535,
            1.0,
        ),
        (Level::Thinking(2), true) => curve(
            [0.57486, -0.01765, -0.58987, 5.25227, -7.27175, 3.25532],
            0.0,
            0.842,
        ),
        (Level::Thinking(2), false) => curve(
            [
                -59.89653, 377.05024, -945.07843, 1182.03145, -734.96375, 181.68658,
            ],
            0.556,
            1.0,
        ),
        (Level::Thinking(3), true) => curve(
            [0.59621, -0.00062, -0.71148, 5.53588, -7.6392, 3.44606],
            0.0,
            0.827,
        ),
        (Level::Thinking(3), false) => curve(
            [
                -72.07122,
                445.84082,
                -1099.57352,
                1353.88793,
                -829.53392,
                202.26164,
            ],
            0.58,
            1.0,
        ),
        (Level::Thinking(4), true) => curve(
            [0.61691, 0.01462, -0.82288, 5.80651, -8.00641, 3.64333],
            0.0,
            0.811,
        ),
        (Level::Thinking(4), false) => curve(
            [
                -110.14338,
                674.21488,
                -1645.75941,
                2004.32367,
                -1215.15899,
                293.3183,
            ],
            0.6,
            1.0,
        ),
        (Level::Thinking(5), true) => curve(
            [0.63702, 0.02826, -0.92498, 6.06465, -8.37246, 3.84651],
            0.0,
            0.795,
        ),
        (Level::Thinking(5), false) => curve(
            [
                -175.47701,
                1063.54495,
                -2570.70594,
                3098.80776,
                -1860.15527,
                444.76392,
            ],
            0.62,
            1.0,
        ),
        (Level::Thinking(_), true) => curve(
            [0.65658, 0.04044, -1.01835, 6.30989, -8.73529, 4.05439],
            0.0,
            0.779,
        ),
        (Level::Thinking(_), false) => curve(
            [
                -183.81712,
                1094.70055,
                -2602.68539,
                3088.71276,
                -1826.91131,
                430.75931,
            ],
            0.643,
            1.0,
        ),
        (Level::Subtle, true) => curve(
            [0.56762, -0.02475, -0.5383, 5.12628, -7.10931, 3.17324],
            0.0,
            0.848,
        ),
        (Level::Subtle, false) => curve(
            [
                -232.85459,
                1376.54473,
                -3249.11801,
                3827.91186,
                -2248.29472,
                526.55751,
            ],
            0.657,
            1.0,
        ),
        (Level::Thumb, true) => curve(
            [0.60323, 0.00278, -0.73328, 5.57157, -7.68067, 3.46933],
            0.0,
            0.823,
        ),
        (Level::Thumb, false) => curve(
            [
                -82.89897,
                511.01355,
                -1255.98095,
                1540.76821,
                -940.68087,
                228.58523,
            ],
            0.586,
            1.0,
        ),
        (Level::Readable, true) => curve(
            [0.66937, 0.04704, -1.06871, 6.43941, -8.9332, 4.17229],
            0.0,
            0.77,
        ),
        (Level::Readable, false) => curve(
            [
                -1554.52576,
                8733.56817,
                -19604.93507,
                21977.72696,
                -12300.99599,
                2749.81288,
            ],
            0.751,
            1.0,
        ),
        (Level::Emphasis, true) => curve(
            [0.7303, 0.07695, -1.31626, 7.1681, -10.14436, 4.92846],
            0.0,
            0.712,
        ),
        (Level::Emphasis, false) => curve(
            [
                -4948.31942,
                26870.91986,
                -58334.48399,
                63280.17197,
                -34298.01053,
                7430.30146,
            ],
            0.811,
            1.0,
        ),
        (Level::TextOnPanel, true) => curve(
            [0.86713, 0.05232, -0.89428, 4.79014, -5.5432, 1.75023],
            0.0,
            0.542,
        ),
        (Level::TextOnPanel, false) => curve(
            [
                -8570.89457,
                43954.60805,
                -90084.00702,
                92220.6791,
                -47152.15802,
                9632.27113,
            ],
            0.867,
            1.0,
        ),
        (Level::Text, true) => curve(
            [0.89242, 0.02311, -0.44862, 2.34417, -0.06084, -2.63844],
            0.0,
            0.5,
        ),
        (Level::Text, false) => curve(
            [
                -2004.67048,
                6664.47299,
                -6060.70202,
                -1792.61209,
                5133.82359,
                -1939.85583,
            ],
            0.894,
            1.0,
        ),
    }
}

const TOOL_PANELS: [&str; 3] = ["toolPendingBg", "toolSuccessBg", "toolErrorBg"];
const MESSAGE_PANELS: [&str; 2] = ["userMessageBg", "customMessageBg"];
const PANELS: [&str; 7] = [
    "userMessageBg",
    "toolPendingBg",
    "toolSuccessBg",
    "toolErrorBg",
    "selectedBg",
    "searchMatchBg",
    "customMessageBg",
];
const THINKING: [&str; 7] = [
    "thinkingOff",
    "thinkingMinimal",
    "thinkingLow",
    "thinkingMedium",
    "thinkingHigh",
    "thinkingXhigh",
    "thinkingMax",
];
const FOREGROUND_TOKENS: [&str; 3] = ["text", "userMessageText", "toolTitle"];
const TEXT_MINIMUM_WCAG_CONTRAST: f64 = 4.5;

struct Rule {
    token: &'static str,
    on: Vec<&'static str>,
    level: Level,
}

fn rules() -> Vec<Rule> {
    let mut rules = Vec::new();
    let mut each = |tokens: &[&'static str], on: Vec<&'static str>, level: Level| {
        for token in tokens {
            rules.push(Rule {
                token,
                on: on.clone(),
                level,
            });
        }
    };
    let with = |head: &[&'static str], tails: &[&[&'static str]]| -> Vec<&'static str> {
        let mut out = head.to_vec();
        for tail in tails {
            out.extend_from_slice(tail);
        }
        out
    };
    each(&PANELS, vec!["background"], Level::Panel);
    each(&["text"], vec!["background"], Level::Text);
    each(&["text"], vec!["selectedBg"], Level::TextOnPanel);
    each(
        &["userMessageText"],
        vec!["userMessageBg"],
        Level::TextOnPanel,
    );
    each(&["toolTitle"], TOOL_PANELS.to_vec(), Level::TextOnPanel);
    each(
        &["accent", "success", "error", "warning"],
        with(&["background", "selectedBg"], &[&TOOL_PANELS]),
        Level::Readable,
    );
    each(
        &["muted"],
        with(
            &["background", "selectedBg", "customMessageBg"],
            &[&TOOL_PANELS],
        ),
        Level::Readable,
    );
    each(
        &["dim"],
        with(
            &["background", "selectedBg", "customMessageBg"],
            &[&TOOL_PANELS],
        ),
        Level::Subtle,
    );
    each(&["thinkingText"], vec!["background"], Level::Readable);
    each(
        &["customMessageText"],
        with(&["customMessageBg"], &[&TOOL_PANELS]),
        Level::Readable,
    );
    each(
        &["customMessageLabel"],
        with(
            &["background", "customMessageBg", "selectedBg"],
            &[&TOOL_PANELS],
        ),
        Level::Readable,
    );
    each(
        &["toolOutput"],
        with(&["background"], &[&TOOL_PANELS]),
        Level::Readable,
    );
    each(
        &[
            "mdHeading",
            "mdLink",
            "mdLinkUrl",
            "mdCode",
            "mdQuote",
            "mdCodeBlockBorder",
            "mdListBullet",
        ],
        with(&["background"], &[&MESSAGE_PANELS]),
        Level::Readable,
    );
    each(
        &["mdCodeBlock"],
        with(&["background"], &[&MESSAGE_PANELS, &TOOL_PANELS]),
        Level::Readable,
    );
    each(
        &["toolDiffAdded", "toolDiffRemoved", "toolDiffContext"],
        with(&["background"], &[&TOOL_PANELS]),
        Level::Readable,
    );
    each(
        &[
            "syntaxComment",
            "syntaxKeyword",
            "syntaxFunction",
            "syntaxVariable",
            "syntaxString",
            "syntaxNumber",
            "syntaxType",
            "syntaxOperator",
            "syntaxPunctuation",
        ],
        with(&["background"], &[&MESSAGE_PANELS, &TOOL_PANELS]),
        Level::Readable,
    );
    each(&["searchMatchText"], vec!["searchMatchBg"], Level::Readable);
    each(
        &["bashMode", "border", "borderAccent"],
        vec!["background"],
        Level::Readable,
    );
    each(&["borderMuted"], vec!["background"], Level::Subtle);
    each(
        &["mdQuoteBorder", "mdHr"],
        with(&["background"], &[&MESSAGE_PANELS, &TOOL_PANELS]),
        Level::Readable,
    );
    each(&["scrollbarTrack"], vec!["background"], Level::Track);
    each(&["scrollbarThumb"], vec!["scrollbarTrack"], Level::Thumb);
    for (index, token) in THINKING.iter().enumerate() {
        each(&[token], vec!["background"], Level::Thinking(index as u8));
    }
    rules
}

fn solve_order(rules: &[Rule]) -> Vec<&'static str> {
    fn visit(token: &'static str, rules: &[Rule], order: &mut Vec<&'static str>) {
        if order.contains(&token) {
            return;
        }
        for rule in rules.iter().filter(|rule| rule.token == token) {
            for surface in &rule.on {
                if *surface != "background" {
                    visit(surface, rules, order);
                }
            }
        }
        order.push(token);
    }
    let mut order = Vec::new();
    for rule in rules {
        visit(rule.token, rules, &mut order);
    }
    order
}

/// What the terminal reported about its colors.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SystemThemeInput {
    /// Default foreground, sRGB 0 to 255.
    pub foreground: Option<[f64; 3]>,
    /// Default background, sRGB 0 to 255.
    pub background: Option<[f64; 3]>,
    /// ANSI colors 0 to 15.
    pub palette: Option<Vec<[f64; 3]>>,
    /// Saturation multiplier, 0 (grayscale, while colors are pending) to 1.
    pub saturation: f64,
    /// Appearance when the terminal did not report its background.
    pub appearance_hint: Option<Appearance>,
}

/// A generated token value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum SystemValue {
    /// The terminal's default color.
    Default,
    /// A palette index the terminal renders itself.
    Index(u8),
    /// A color.
    Rgb([f64; 3]),
}

/// The generated system theme.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SystemThemeColors {
    /// Token values, in pi's token order.
    pub colors: Vec<(&'static str, SystemValue)>,
    /// Foreground tokens rendered faint.
    pub dim: Vec<&'static str>,
    /// The terminal's appearance.
    pub appearance: Option<Appearance>,
}

fn rgb_color(rgb: [f64; 3]) -> Color {
    Color::Rgb(rgb[0], rgb[1], rgb[2])
}

fn oklab_lightness(rgb: [f64; 3]) -> f64 {
    rgb_color(rgb).to_oklch().0
}

fn relative_luminance(rgb: [f64; 3]) -> f64 {
    let linear = |channel: f64| crate::color::srgb_to_linear(channel / 255.0);
    0.2126 * linear(rgb[0]) + 0.7152 * linear(rgb[1]) + 0.0722 * linear(rgb[2])
}

/// WCAG 2 contrast ratio, 1 to 21.
pub(crate) fn wcag_contrast(first: [f64; 3], second: [f64; 3]) -> f64 {
    let (a, b) = (relative_luminance(first), relative_luminance(second));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// Whether a terminal with these colors is dark or light.
pub fn terminal_appearance(background: [f64; 3], foreground: Option<[f64; 3]>) -> Appearance {
    let white = wcag_contrast([255.0; 3], background);
    let black = wcag_contrast([0.0; 3], background);
    if let Some(foreground) = foreground {
        let (fl, bl) = (oklab_lightness(foreground), oklab_lightness(background));
        if (fl - bl).abs() > 0.05 {
            let dark = fl > bl;
            let best = if dark { white } else { black };
            if best >= TEXT_MINIMUM_WCAG_CONTRAST {
                return if dark {
                    Appearance::Dark
                } else {
                    Appearance::Light
                };
            }
        }
    }
    if white >= black {
        Appearance::Dark
    } else {
        Appearance::Light
    }
}

fn bell_weight(lightness: f64) -> f64 {
    let gaussian = |x: f64| (-((x - 0.5).powi(2)) / (2.0 * 0.25f64.powi(2))).exp();
    (gaussian(lightness) - gaussian(0.0)) / (1.0 - gaussian(0.0))
}

fn saturation_curve(family: &Family, lightness: f64) -> f64 {
    let floor = if family.max > 0.0 {
        family.min / family.max
    } else {
        1.0
    };
    floor + (1.0 - floor) * bell_weight(lightness)
}

fn level_target(level: Level, appearance: Appearance, surface: f64) -> Option<f64> {
    let curve = level_curve(level, appearance);
    if surface < curve.reachable.0 || surface > curve.reachable.1 {
        return None;
    }
    Some(
        curve
            .coefficients
            .iter()
            .enumerate()
            .map(|(power, coefficient)| coefficient * surface.powi(power as i32))
            .sum(),
    )
}

#[derive(Clone, Copy)]
struct Source {
    h: f64,
    s: f64,
    l: f64,
    chroma: f64,
}

fn source_of(rgb: [f64; 3]) -> Source {
    let (h, s, l) = crate::color::rgb_to_okhsl(rgb);
    Source {
        h,
        s,
        l,
        chroma: rgb_color(rgb).to_oklch().1,
    }
}

fn okhsl_rgb(h: f64, s: f64, l: f64) -> [f64; 3] {
    okhsl_to_rgb(h, s.clamp(0.0, 1.0), l.clamp(0.0, 1.0))
}

fn anchored(source: Source, family: &Family, lightness: f64, saturation: f64) -> [f64; 3] {
    let anchor = saturation_curve(family, source.l);
    let falloff = if anchor > 0.0 {
        (saturation_curve(family, lightness) / anchor).min(1.0)
    } else {
        1.0
    };
    let color = okhsl_rgb(source.h, source.s * falloff * saturation, lightness);
    let cap = source.chroma * falloff * saturation;
    let (l, c, _) = rgb_color(color).to_oklch();
    if c <= cap {
        color
    } else {
        Color::Oklch(l, cap, ((source.h % 360.0) + 360.0) % 360.0).to_rgb()
    }
}

fn with_text_contrast(color: [f64; 3], surfaces: &[[f64; 3]], lighter: bool) -> [f64; 3] {
    let meets = |candidate: [f64; 3]| {
        surfaces
            .iter()
            .all(|surface| wcag_contrast(candidate, *surface) >= TEXT_MINIMUM_WCAG_CONTRAST)
    };
    if meets(color) {
        return color;
    }
    let (h, s, l) = crate::color::rgb_to_okhsl(color);
    let at = |lightness: f64| okhsl_rgb(h, s, lightness);
    let extreme = if lighter { 1.0 } else { 0.0 };
    if !meets(at(extreme)) {
        return at(extreme);
    }
    at(bisect(extreme, l, |lightness| meets(at(lightness))))
}

fn indexed_colors(saturation: f64, appearance: Option<Appearance>) -> SystemThemeColors {
    let mut colors = Vec::new();
    let mut dim = Vec::new();
    for (token, family) in TOKEN_FAMILIES {
        if PANELS.contains(&token) {
            colors.push((token, SystemValue::Default));
            continue;
        }
        let neutral = std::ptr::eq(family, &NEUTRAL);
        let value = if !neutral && saturation > 0.0 {
            SystemValue::Index(token_slot(token) as u8)
        } else {
            SystemValue::Default
        };
        colors.push((token, value));
        if neutral && !FOREGROUND_TOKENS.contains(&token) {
            dim.push(token);
        }
    }
    SystemThemeColors {
        colors,
        dim,
        appearance,
    }
}

/// Generates the system theme's colors.
pub(crate) fn generate_system_theme(input: &SystemThemeInput) -> SystemThemeColors {
    let saturation = input.saturation.clamp(0.0, 1.0);
    let Some(background) = input.background else {
        return indexed_colors(saturation, input.appearance_hint);
    };
    let foreground = input.foreground;
    let palette: Option<Vec<Source>> = input
        .palette
        .as_ref()
        .filter(|palette| palette.len() == 16)
        .map(|palette| palette.iter().copied().map(source_of).collect());
    let appearance = terminal_appearance(background, foreground);
    let lighter = appearance == Appearance::Dark;
    let extreme = if lighter { 1.0 } else { 0.0 };
    let background_l = oklab_lightness(background);
    let readable_floor = if lighter {
        Level::Readable
    } else {
        Level::Subtle
    };
    let rules = rules();
    let order = solve_order(&rules);

    let paint = |token: &str, oklab_l: f64| -> [f64; 3] {
        let lightness = oklab_to_okhsl_lightness(oklab_l);
        let family = token_family(token);
        match &palette {
            None => okhsl_rgb(
                family.hue,
                (family.min + (family.max - family.min) * bell_weight(lightness)) * saturation,
                lightness,
            ),
            Some(palette) => anchored(palette[token_slot(token)], family, lightness, saturation),
        }
    };
    let target = |level: Level, surface: f64, t: f64| -> Option<f64> {
        let reached = level_target(level, appearance, surface);
        if reached.is_none() && t == 0.0 {
            return None;
        }
        let distance = reached.unwrap_or(extreme) - surface;
        let floor = level_target(readable_floor, appearance, surface).unwrap_or(extreme) - surface;
        let compressed = if distance.abs() > floor.abs() {
            distance - (distance - floor) * t.min(1.0)
        } else {
            distance
        };
        Some(surface + compressed * (1.0 - (t - 1.0).max(0.0)))
    };
    let extreme_text = if lighter { [255.0; 3] } else { [0.0; 3] };
    let readable =
        |color: [f64; 3]| wcag_contrast(extreme_text, color) >= TEXT_MINIMUM_WCAG_CONTRAST;
    let limit_panel = |token: &str, l: f64| -> [f64; 3] {
        let color = paint(token, l);
        if readable(color) {
            return color;
        }
        paint(
            token,
            bisect(background_l, l, |lightness| {
                readable(paint(token, lightness))
            }),
        )
    };
    let solve = |t: f64| -> Option<HashMap<&'static str, [f64; 3]>> {
        let mut colors: HashMap<&'static str, [f64; 3]> = HashMap::new();
        colors.insert("background", background);
        for token in &order {
            let mut targets = Vec::new();
            for rule in rules.iter().filter(|rule| rule.token == *token) {
                for surface in &rule.on {
                    let surface_l =
                        oklab_lightness(colors.get(surface).copied().unwrap_or(background));
                    let value = target(rule.level, surface_l, t)?;
                    if !(0.0..=1.0).contains(&value) {
                        return None;
                    }
                    targets.push(value);
                }
            }
            let l = if lighter {
                targets.iter().copied().fold(f64::NEG_INFINITY, f64::max)
            } else {
                targets.iter().copied().fold(f64::INFINITY, f64::min)
            };
            let color = if PANELS.contains(token) {
                limit_panel(token, l)
            } else {
                paint(token, l)
            };
            colors.insert(token, color);
        }
        Some(colors)
    };

    let mut relaxation = 0.0;
    let mut solved = solve(0.0);
    if solved.is_none() {
        relaxation = bisect(2.0, 0.0, |t| solve(t).is_some());
        solved = solve(relaxation);
    }
    let solved = solved.unwrap_or_default();
    let surfaces_of = |token: &str| -> Vec<[f64; 3]> {
        rules
            .iter()
            .filter(|rule| rule.token == token)
            .flat_map(|rule| {
                rule.on
                    .iter()
                    .map(|surface| solved.get(surface).copied().unwrap_or(background))
            })
            .collect()
    };

    let mut colors: Vec<(&'static str, SystemValue)> = TOKEN_FAMILIES
        .iter()
        .map(|(token, _)| {
            let value = solved.get(token).map_or(SystemValue::Default, |rgb| {
                SystemValue::Rgb(rgb.map(|c| c.round()))
            });
            (*token, value)
        })
        .collect();
    for token in FOREGROUND_TOKENS {
        let surfaces = surfaces_of(token);
        let mut text = solved.get(token).copied();
        if let Some(foreground) = foreground {
            let targets: Vec<Option<f64>> = surfaces
                .iter()
                .map(|surface| target(Level::Emphasis, oklab_lightness(*surface), relaxation))
                .collect();
            if targets
                .iter()
                .all(|value| value.is_some_and(|v| (0.0..=1.0).contains(&v)))
            {
                let values = targets.iter().flatten().copied();
                let needed = if lighter {
                    values.fold(f64::NEG_INFINITY, f64::max)
                } else {
                    values.fold(f64::INFINITY, f64::min)
                };
                let foreground_l = oklab_lightness(foreground);
                if (lighter && foreground_l >= needed) || (!lighter && foreground_l <= needed) {
                    set(&mut colors, token, SystemValue::Default);
                    continue;
                }
                text = Some(anchored(
                    source_of(foreground),
                    &NEUTRAL,
                    oklab_to_okhsl_lightness(needed),
                    saturation,
                ));
            }
        }
        if let Some(text) = text {
            let rgb = with_text_contrast(text, &surfaces, lighter);
            set(&mut colors, token, SystemValue::Rgb(rgb.map(|c| c.round())));
        }
    }
    SystemThemeColors {
        colors,
        dim: Vec::new(),
        appearance: Some(appearance),
    }
}

fn set(colors: &mut [(&'static str, SystemValue)], token: &str, value: SystemValue) {
    if let Some(entry) = colors.iter_mut().find(|(name, _)| *name == token) {
        entry.1 = value;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// System theme colors against pi's, recorded by
    /// `tests/fixtures/pi/generator/theme.mjs`.
    fn fixture() -> Value {
        let text = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/pi/theme/theme.json"
        ))
        .unwrap();
        serde_json::from_str(&text).unwrap()
    }

    fn rgb(value: &Value) -> Option<[f64; 3]> {
        let value = value.as_object()?;
        Some(["r", "g", "b"].map(|channel| value[channel].as_f64().unwrap()))
    }

    #[test]
    fn system_themes_match_pi() {
        let fixture = fixture();
        let mut failures = Vec::new();
        for (name, case) in fixture["system"].as_object().unwrap() {
            let input = &case["input"];
            let generated = generate_system_theme(&SystemThemeInput {
                foreground: rgb(&input["foreground"]),
                background: rgb(&input["background"]),
                palette: input["palette"]
                    .as_array()
                    .map(|palette| palette.iter().map(|entry| rgb(entry).unwrap()).collect()),
                saturation: input["saturation"].as_f64().unwrap_or(1.0),
                appearance_hint: match input["appearanceHint"].as_str() {
                    Some("light") => Some(Appearance::Light),
                    Some("dark") => Some(Appearance::Dark),
                    _ => None,
                },
            });
            for (token, value) in case["colors"].as_object().unwrap() {
                let expected = match value {
                    Value::Number(index) => SystemValue::Index(index.as_u64().unwrap() as u8),
                    Value::String(text) if text.is_empty() => SystemValue::Default,
                    Value::String(hex) => SystemValue::Rgb(
                        [1, 3, 5]
                            .map(|at| f64::from(u8::from_str_radix(&hex[at..at + 2], 16).unwrap())),
                    ),
                    other => panic!("unexpected value {other}"),
                };
                let actual = generated
                    .colors
                    .iter()
                    .find(|(t, _)| t == token)
                    .map(|(_, value)| *value);
                if actual != Some(expected) {
                    failures.push(format!("{name} {token}: pi {expected:?}, yapi {actual:?}"));
                }
            }
            let dim: Vec<&str> = case["dim"]
                .as_array()
                .unwrap()
                .iter()
                .map(|token| token.as_str().unwrap())
                .collect();
            if generated.dim != dim {
                failures.push(format!("{name} dim: pi {dim:?}, yapi {:?}", generated.dim));
            }
            let appearance = match case["appearance"].as_str() {
                Some("dark") => Some(Appearance::Dark),
                Some("light") => Some(Appearance::Light),
                _ => None,
            };
            if generated.appearance != appearance {
                failures.push(format!("{name} appearance"));
            }
        }
        assert!(
            failures.is_empty(),
            "{} differences:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }

    #[test]
    fn loads_builtin_themes() {
        let dark = Theme::builtin("dark", ColorMode::TrueColor).unwrap();
        assert_eq!(dark.appearance, Some(Appearance::Dark));
        assert!(matches!(dark.paint("thinkingMax"), Some(Paint::Color(_))));
        assert_eq!(dark.paint("searchMatchText"), dark.paint("muted"));
        let light = Theme::builtin("light", ColorMode::Ansi256).unwrap();
        assert!(matches!(
            light.fg("accent").fg,
            Some(ratatui_core::style::Color::Indexed(_))
        ));
    }

    #[test]
    fn rejects_bad_documents() {
        let error = Theme::from_json("x", r#"{"colors":{"accent":"nope"}}"#, ColorMode::TrueColor)
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with(
                "Invalid theme \"x\":\n\nMissing required color tokens:\n  - bashMode\n  - border\n"
            ),
            "{error}"
        );
        assert!(error.ends_with("for reference values."), "{error}");
        // Registration resolves colors first, as pi's resource loader does.
        let error =
            Theme::from_json_lenient("x", r#"{"colors":{"accent":"nope"}}"#, ColorMode::TrueColor)
                .unwrap_err();
        assert_eq!(error.to_string(), "Variable reference not found: nope");
        let mut colors: Map<String, Value> = REQUIRED_TOKENS
            .iter()
            .map(|token| ((*token).to_owned(), Value::from("a")))
            .collect();
        colors.insert("text".into(), Value::from("b"));
        let doc = serde_json::json!({"vars": {"a": "b", "b": "a"}, "colors": colors}).to_string();
        assert!(matches!(
            Theme::from_json("x", &doc, ColorMode::TrueColor),
            Err(ThemeError::Circular(_))
        ));
    }

    #[test]
    fn reads_settings_and_environment() {
        assert_eq!(
            resolve_theme_setting(Some("light/dark"), Appearance::Light),
            Some("light".into())
        );
        assert_eq!(resolve_theme_setting(Some("a/b/c"), Appearance::Dark), None);
        assert_eq!(detect_colorfgbg(Some("15;0")), Some(Appearance::Dark));
        assert_eq!(
            detect_colorfgbg(Some("0;default;15")),
            Some(Appearance::Light)
        );
        assert_eq!(detect_colorfgbg(Some("default")), None);
    }
}
