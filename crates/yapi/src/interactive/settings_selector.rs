//! The `/settings` selector: one list of settings, with submenus for
//! warnings, per-model thinking levels and the theme.
//!
//! Ports of `settings-selector.ts` and `settings-submenu.ts` in
//! `packages/coding-agent/src/modes/interactive/components` in pi `v1.0.0`.

use ratatui_core::style::Modifier;
use yapi_tui::fuzzy::fuzzy_filter;
use yapi_tui::lines::{self, StyledLine, styled};
use yapi_tui::select_list::{SelectEvent, SelectItem, SelectList, SelectListLayout};
use yapi_tui::settings_list::{SettingItem, SettingsEvent, SettingsList, SettingsListTheme};
use yapi_tui::text_input::TextInput;
use yapi_types::message::ThinkingLevel;
use yapi_types::model::Model;
use yapi_types::settings::Settings;

use super::keybindings::keys_display;
use super::selectors::{Action, Outcome, Ui, level_description};

const SUBMENU_LAYOUT: SelectListLayout = SelectListLayout {
    min_primary_column_width: Some(12),
    max_primary_column_width: Some(32),
};

const MODEL_PICKER_LAYOUT: SelectListLayout = SelectListLayout {
    min_primary_column_width: Some(12),
    max_primary_column_width: Some(46),
};

/// pi's HTTP idle timeout choices: label and milliseconds.
pub const HTTP_IDLE_TIMEOUTS: [(&str, u64); 5] = [
    ("30 sec", 30_000),
    ("1 min", 60_000),
    ("2 min", 120_000),
    ("5 min", 300_000),
    ("disabled", 0),
];

const CLEAR_OVERRIDE: &str = "__clear__";
const NO_MODELS: &str = "__none__";
const AUTOMATIC_THEME: &str = "/";

/// pi's `formatHttpIdleTimeoutMs`.
pub fn format_http_idle_timeout(ms: u64) -> String {
    HTTP_IDLE_TIMEOUTS
        .iter()
        .find(|(_, value)| *value == ms)
        .map_or_else(
            || format!("{} sec", yapi_core::tools::js_number(ms as f64 / 1000.0)),
            |(label, _)| (*label).to_owned(),
        )
}

/// pi's labels for `defaultProjectTrust`.
pub fn project_trust_label(value: &str) -> &'static str {
    match value {
        "always" => "Always trust",
        "never" => "Never trust",
        _ => "Ask",
    }
}

/// What the selector shows, read when it opens.
pub struct SettingsConfig {
    /// The merged settings.
    pub settings: Settings,
    /// Whether automatic compaction runs.
    pub auto_compact: bool,
    /// The session's queue modes.
    pub steering_mode: String,
    /// The session's queue modes.
    pub follow_up_mode: String,
    /// The HTTP idle timeout in milliseconds.
    pub http_idle_timeout_ms: u64,
    /// Thinking blocks are hidden.
    pub hide_thinking: bool,
    /// The current TUI mode.
    pub fullscreen: bool,
    /// Models with credentials.
    pub models: Vec<Model>,
    /// The session's model.
    pub current_model: Option<(String, String)>,
    /// The global default thinking level.
    pub default_thinking: ThinkingLevel,
    /// The `theme` setting, or `system`.
    pub theme: String,
    /// Available theme names, `system` first.
    pub themes: Vec<String>,
    /// The terminal is light.
    pub light_terminal: bool,
}

fn flag(value: bool) -> String {
    if value { "true" } else { "false" }.to_owned()
}

fn item(id: &str, label: &str, description: &str, value: String, values: &[&str]) -> SettingItem {
    SettingItem {
        id: id.to_owned(),
        label: label.to_owned(),
        description: Some(description.to_owned()),
        current_value: value,
        values: values.iter().map(|value| (*value).to_owned()).collect(),
        submenu: false,
    }
}

fn submenu_item(id: &str, label: &str, description: &str, value: String) -> SettingItem {
    SettingItem {
        submenu: true,
        ..item(id, label, description, value, &[])
    }
}

fn model_key(model: &Model) -> String {
    format!("{}/{}", model.provider, model.id)
}

fn overrides_summary(count: usize) -> String {
    if count == 0 {
        "none".to_owned()
    } else {
        format!("{count} configured")
    }
}

fn list_theme(ui: &Ui<'_>) -> SettingsListTheme {
    SettingsListTheme {
        selected: ui.theme.fg("accent"),
        value: ui.theme.fg("muted"),
        description: ui.theme.fg("dim"),
        hint: ui.theme.fg("dim"),
    }
}

fn text_rows(text: &str, style: ratatui_core::style::Style, width: usize) -> Vec<StyledLine> {
    lines::text(&[styled(text.to_owned(), style)], width, 0, 0, None)
}

/// pi's `SelectSubmenu`: a titled select list, optionally filtered by typing.
struct SelectSubmenu {
    title: String,
    description: String,
    items: Vec<SelectItem>,
    /// What typing matches each item against.
    search_texts: Vec<String>,
    list: SelectList,
    search: Option<TextInput>,
    layout: SelectListLayout,
}

enum SubmenuEvent {
    None,
    Moved(String),
    Selected(String),
    Cancelled,
}

impl SelectSubmenu {
    fn new(
        title: String,
        description: String,
        items: Vec<SelectItem>,
        current: &str,
        searchable: bool,
        layout: SelectListLayout,
        ui: &Ui<'_>,
    ) -> SelectSubmenu {
        let list = Self::list(items.clone(), current, layout, ui);
        let search_texts = items
            .iter()
            .map(|item| {
                format!(
                    "{} {}",
                    item.label,
                    item.description.as_deref().unwrap_or_default()
                )
            })
            .collect();
        SelectSubmenu {
            title,
            description,
            items,
            search_texts,
            list,
            search: searchable.then(TextInput::default),
            layout,
        }
    }

    fn list(
        items: Vec<SelectItem>,
        current: &str,
        layout: SelectListLayout,
        ui: &Ui<'_>,
    ) -> SelectList {
        let index = items.iter().position(|item| item.value == current);
        let mut list = SelectList::new(
            items.clone(),
            items.len().min(10),
            ui.select_list_theme(),
            layout,
        );
        if let Some(index) = index {
            list.set_selected_index(index);
        }
        list
    }

    fn render(&mut self, width: usize, ui: &Ui<'_>) -> Vec<StyledLine> {
        let theme = ui.theme;
        let mut out = text_rows(
            &self.title,
            theme.fg("accent").add_modifier(Modifier::BOLD),
            width,
        );
        if !self.description.is_empty() {
            out.extend(lines::spacer(1));
            out.extend(text_rows(&self.description, theme.fg("muted"), width));
        }
        if let Some(input) = &mut self.search {
            out.extend(lines::spacer(1));
            out.push(input.render(width));
        }
        out.extend(lines::spacer(1));
        out.extend(self.list.render(width));
        out.extend(lines::spacer(1));
        let hint = if self.search.is_some() {
            "  Type to filter · Enter to select · Esc to go back"
        } else {
            "  Enter to select · Esc to go back"
        };
        out.extend(text_rows(hint, theme.fg("dim"), width));
        out
    }

    fn handle_input(&mut self, data: &str, ui: &Ui<'_>) -> SubmenuEvent {
        let kb = ui.keys;
        let navigation = [
            "tui.select.up",
            "tui.select.down",
            "tui.select.confirm",
            "tui.select.cancel",
        ]
        .iter()
        .any(|action| kb.matches(data, action));
        if let Some(input) = &mut self.search
            && !navigation
        {
            input.handle_input(data, kb);
            let query = input.value().to_owned();
            let items = if query.is_empty() {
                self.items.clone()
            } else {
                let indices: Vec<usize> = (0..self.items.len()).collect();
                fuzzy_filter(indices, &query, |index| self.search_texts[*index].clone())
                    .into_iter()
                    .map(|index| self.items[index].clone())
                    .collect()
            };
            self.list = Self::list(items, "", self.layout, ui);
            return SubmenuEvent::None;
        }
        match self.list.handle_input(data, kb) {
            SelectEvent::Selected(item) => SubmenuEvent::Selected(item.value),
            SelectEvent::Cancelled => SubmenuEvent::Cancelled,
            SelectEvent::Moved => self
                .list
                .selected_item()
                .map_or(SubmenuEvent::None, |item| {
                    SubmenuEvent::Moved(item.value.clone())
                }),
            SelectEvent::Ignored => SubmenuEvent::None,
        }
    }
}

/// pi's per-model thinking level `SteppedSubmenu`: a model, then a level,
/// looping back to the models.
struct ModelThinking {
    models: Vec<Model>,
    current: Option<String>,
    default: Option<String>,
    overrides: Vec<(String, ThinkingLevel)>,
    global: ThinkingLevel,
    model: Option<String>,
    menu: SelectSubmenu,
}

impl ModelThinking {
    fn override_of(&self, key: &str) -> Option<ThinkingLevel> {
        self.overrides
            .iter()
            .find(|(model, _)| model == key)
            .map(|(_, level)| *level)
    }

    fn models_step(&self, ui: &Ui<'_>) -> SelectSubmenu {
        let rank = |model: &Model| {
            let key = model_key(model);
            if Some(&key) == self.current.as_ref() {
                0
            } else if Some(&key) == self.default.as_ref() {
                1
            } else {
                2
            }
        };
        let mut sorted = self.models.clone();
        sorted.sort_by(|a, b| {
            rank(a)
                .cmp(&rank(b))
                .then_with(|| match (rank(a), rank(b)) {
                    (2, 2) => a.provider.cmp(&b.provider),
                    _ => std::cmp::Ordering::Equal,
                })
        });
        let mut items: Vec<SelectItem> = sorted
            .iter()
            .map(|model| {
                let key = model_key(model);
                SelectItem {
                    description: self
                        .override_of(&key)
                        .map(|level| level.as_str().to_owned()),
                    label: format!("{} [{}]", model.id, model.provider),
                    value: key,
                }
            })
            .collect();
        if items.is_empty() {
            items.push(SelectItem {
                value: NO_MODELS.to_owned(),
                label: "No models available".to_owned(),
                description: Some("Log in to a provider or configure an API key first".to_owned()),
            });
        }
        let preselect = self
            .current
            .clone()
            .or_else(|| self.default.clone())
            .unwrap_or_default();
        // pi's labels color the provider, and its filter matches the color
        // codes too.
        let muted = yapi_tui::ansi::sgr(ui.theme.fg("muted"));
        let search_texts = items
            .iter()
            .map(|item| {
                let label = match item.label.rsplit_once(" [") {
                    Some((id, provider)) if item.value != NO_MODELS => {
                        format!("{id} {muted}[{provider}\x1b[39m")
                    }
                    _ => item.label.clone(),
                };
                format!(
                    "{label} {}",
                    item.description.as_deref().unwrap_or_default()
                )
            })
            .collect();
        let mut menu = SelectSubmenu::new(
            "Per-Model Thinking Level".to_owned(),
            "Step 1/2 · Select a model to configure".to_owned(),
            items,
            &preselect,
            true,
            MODEL_PICKER_LAYOUT,
            ui,
        );
        menu.search_texts = search_texts;
        menu
    }

    fn levels_step(&self, key: &str, ui: &Ui<'_>) -> SelectSubmenu {
        let model = self.models.iter().find(|model| model_key(model) == key);
        let title = match model {
            Some(model) => format!("Thinking Level for {} [{}]", model.id, model.provider),
            None => format!("Thinking Level for {key}"),
        };
        let active = self.override_of(key);
        let mut items: Vec<SelectItem> = match model {
            Some(model) if model.reasoning => yapi_ai::thinking::supported_levels(model),
            Some(_) => vec![ThinkingLevel::Off],
            None => Vec::new(),
        }
        .into_iter()
        .map(|level| SelectItem {
            value: level.as_str().to_owned(),
            label: format!(
                "{}{}",
                if Some(level) == active { "✓ " } else { "  " },
                level.as_str()
            ),
            description: Some(level_description(level).to_owned()),
        })
        .collect();
        if model.is_some() && active.is_some() {
            items.push(SelectItem {
                value: CLEAR_OVERRIDE.to_owned(),
                label: "  (clear override)".to_owned(),
                description: Some(format!(
                    "Revert to global default ({})",
                    self.global.as_str()
                )),
            });
        }
        let preselect = active
            .map(|level| level.as_str().to_owned())
            .unwrap_or_default();
        SelectSubmenu::new(
            title,
            "Step 2/2 · Select default thinking level for this model".to_owned(),
            items,
            &preselect,
            false,
            SUBMENU_LAYOUT,
            ui,
        )
    }
}

/// pi's `ThemeSubmenu`: one theme, or a light and a dark one for automatic
/// mode.
struct ThemeMenu {
    themes: Vec<String>,
    light_terminal: bool,
    original: String,
    automatic: bool,
    single: String,
    light: String,
    dark: String,
    /// The single-theme list, or a light or dark theme list over the
    /// automatic settings.
    select: Option<(Option<&'static str>, SelectSubmenu)>,
    list: SettingsList,
}

/// What the theme menu asks for.
enum ThemeEvent {
    None,
    Preview(String),
    Done(Option<String>),
}

fn parse_auto_theme(setting: &str) -> Option<(String, String)> {
    let (light, dark) = setting.split_once('/')?;
    let (light, dark) = (light.trim(), dark.trim());
    (!light.is_empty() && !dark.is_empty() && !dark.contains('/'))
        .then(|| (light.to_owned(), dark.to_owned()))
}

fn preferred_theme(themes: &[String], preferred: Option<&str>, fallback: &str) -> String {
    match preferred {
        Some(name) if themes.iter().any(|theme| theme == name) => name.to_owned(),
        _ if themes.iter().any(|theme| theme == fallback) => fallback.to_owned(),
        _ => themes
            .first()
            .cloned()
            .unwrap_or_else(|| fallback.to_owned()),
    }
}

fn theme_items(themes: &[String], current: &str) -> Vec<SelectItem> {
    themes
        .iter()
        .map(|name| SelectItem {
            value: name.clone(),
            label: format!("{}{name}", if name == current { "✓ " } else { "  " }),
            description: (name == yapi_tui::theme::SYSTEM_THEME_NAME)
                .then(|| "Theme created from your terminal's colors".to_owned()),
        })
        .collect()
}

impl ThemeMenu {
    fn new(setting: &str, themes: Vec<String>, light_terminal: bool, ui: &Ui<'_>) -> ThemeMenu {
        let system = yapi_tui::theme::SYSTEM_THEME_NAME;
        let auto = parse_auto_theme(setting);
        let (light, dark) = auto.clone().unwrap_or_else(|| {
            let fixed = (!setting.contains('/')).then_some(setting);
            let name = preferred_theme(&themes, fixed, system);
            (name.clone(), name)
        });
        let active = if light_terminal { &light } else { &dark };
        let fixed = (auto.is_none() && !setting.contains('/')).then_some(setting);
        let single = preferred_theme(
            &themes,
            fixed.or(auto.as_ref().map(|_| active.as_str())),
            system,
        );
        let mut menu = ThemeMenu {
            themes,
            light_terminal,
            original: setting.to_owned(),
            automatic: auto.is_some(),
            single,
            light,
            dark,
            select: None,
            list: SettingsList::new(Vec::new(), 1, list_theme(ui), false),
        };
        if menu.automatic {
            menu.show_automatic(ui);
        } else {
            menu.show_single(ui);
        }
        menu
    }

    fn automatic_setting(&self) -> String {
        format!("{}/{}", self.light, self.dark)
    }

    fn setting(&self) -> String {
        if self.automatic {
            self.automatic_setting()
        } else {
            self.single.clone()
        }
    }

    fn show_single(&mut self, ui: &Ui<'_>) {
        self.automatic = false;
        let mut items = theme_items(&self.themes, &self.single);
        let system = items
            .iter()
            .position(|item| item.value == yapi_tui::theme::SYSTEM_THEME_NAME)
            .map(|index| items.remove(index));
        let automatic = SelectItem {
            value: AUTOMATIC_THEME.to_owned(),
            label: "  automatic".to_owned(),
            description: Some(
                "Use separate themes for light and dark terminal appearance".to_owned(),
            ),
        };
        let items: Vec<SelectItem> = system
            .into_iter()
            .chain(std::iter::once(automatic))
            .chain(items)
            .collect();
        self.select = Some((
            None,
            SelectSubmenu::new(
                "Theme".to_owned(),
                "Select a theme, or choose automatic to follow terminal appearance.".to_owned(),
                items,
                &self.single,
                false,
                SUBMENU_LAYOUT,
                ui,
            ),
        ));
    }

    fn show_automatic(&mut self, ui: &Ui<'_>) {
        self.automatic = true;
        self.select = None;
        let items = vec![
            submenu_item(
                "light-theme",
                "Light theme",
                "Theme to use in automatic mode when the terminal is light",
                self.light.clone(),
            ),
            submenu_item(
                "dark-theme",
                "Dark theme",
                "Theme to use in automatic mode when the terminal is dark",
                self.dark.clone(),
            ),
            item(
                "apply",
                "Apply",
                "Save and go back",
                "save and go back".to_owned(),
                &["save and go back"],
            ),
            item(
                "single-mode",
                "Change mode",
                "Switch to one theme for light and dark",
                "switch to single theme".to_owned(),
                &["switch to single theme"],
            ),
        ];
        self.list = SettingsList::new(items, 4, list_theme(ui), false);
    }

    fn render(&mut self, width: usize, ui: &Ui<'_>) -> Vec<StyledLine> {
        if let Some((None, select)) = &mut self.select {
            return select.render(width, ui);
        }
        let theme = ui.theme;
        let mut out = text_rows(
            "Automatic Theme",
            theme.fg("accent").add_modifier(Modifier::BOLD),
            width,
        );
        out.extend(lines::spacer(1));
        out.extend(text_rows(
            "Choose themes for terminal light and dark appearance.",
            theme.fg("muted"),
            width,
        ));
        out.extend(text_rows(
            "Light/dark detection requires terminal support.",
            theme.fg("muted"),
            width,
        ));
        out.extend(lines::spacer(1));
        // A light or dark theme list takes the settings' place.
        match &mut self.select {
            Some((_, select)) => out.extend(select.render(width, ui)),
            None => out.extend(self.list.render(width)),
        }
        out
    }

    fn handle_input(&mut self, data: &str, ui: &Ui<'_>) -> ThemeEvent {
        if let Some((side, select)) = &mut self.select {
            let side = *side;
            return match select.handle_input(data, ui) {
                SubmenuEvent::None => ThemeEvent::None,
                SubmenuEvent::Moved(value) => ThemeEvent::Preview(match side {
                    None if value == AUTOMATIC_THEME => self.automatic_setting(),
                    _ => value,
                }),
                SubmenuEvent::Selected(value) => match side {
                    None if value == AUTOMATIC_THEME => {
                        self.automatic = true;
                        let preview = self.setting();
                        self.show_automatic(ui);
                        ThemeEvent::Preview(preview)
                    }
                    None => {
                        self.single.clone_from(&value);
                        ThemeEvent::Done(Some(value))
                    }
                    Some(id) => {
                        if id == "light-theme" {
                            self.light.clone_from(&value);
                        } else {
                            self.dark.clone_from(&value);
                        }
                        self.list.update_value(id, &value);
                        self.select = None;
                        ThemeEvent::Preview(self.setting())
                    }
                },
                SubmenuEvent::Cancelled => match side {
                    None => ThemeEvent::Done(None),
                    Some(_) => {
                        self.select = None;
                        ThemeEvent::Preview(self.setting())
                    }
                },
            };
        }
        match self.list.handle_input(data, ui.keys) {
            SettingsEvent::Open(id) => {
                let (title, description, current, side) = if id == "light-theme" {
                    (
                        "Light Theme",
                        "Select the theme to use for light terminal appearance",
                        self.light.clone(),
                        "light-theme",
                    )
                } else {
                    (
                        "Dark Theme",
                        "Select the theme to use for dark terminal appearance",
                        self.dark.clone(),
                        "dark-theme",
                    )
                };
                let select = SelectSubmenu::new(
                    title.to_owned(),
                    description.to_owned(),
                    theme_items(&self.themes, &current),
                    &current,
                    false,
                    SUBMENU_LAYOUT,
                    ui,
                );
                self.select = Some((Some(side), select));
                ThemeEvent::None
            }
            SettingsEvent::Changed { id, .. } if id == "apply" => {
                ThemeEvent::Done(Some(self.automatic_setting()))
            }
            SettingsEvent::Changed { id, .. } if id == "single-mode" => {
                self.single = if self.light_terminal {
                    self.light.clone()
                } else {
                    self.dark.clone()
                };
                let preview = self.single.clone();
                self.show_single(ui);
                ThemeEvent::Preview(preview)
            }
            SettingsEvent::Cancelled => ThemeEvent::Done(None),
            _ => ThemeEvent::None,
        }
    }
}

enum Submenu {
    Warnings(Box<SettingsList>),
    ModelThinking(Box<ModelThinking>),
    Theme(Box<ThemeMenu>),
}

/// The `/settings` selector.
pub struct SettingsSelector {
    list: SettingsList,
    submenu: Option<Submenu>,
    config: SettingsConfig,
    overrides: Vec<(String, ThinkingLevel)>,
    anthropic_extra_usage: bool,
}

impl SettingsSelector {
    /// The selector over `config`.
    pub fn new(config: SettingsConfig, ui: &Ui<'_>) -> SettingsSelector {
        let settings = &config.settings;
        let terminal = settings.terminal.clone().unwrap_or_default();
        let images = settings.images.clone().unwrap_or_default();
        let json = |value: Option<serde_json::Value>, default: &str| {
            value
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_else(|| default.to_owned())
        };
        let follow_up_key = keys_display(ui.keys, "app.message.followUp");
        let cycle_key = keys_display(ui.keys, "app.thinking.cycle");
        let overrides: Vec<(String, ThinkingLevel)> = settings
            .model_thinking_levels
            .clone()
            .unwrap_or_default()
            .into_iter()
            .collect();
        let quiet = match serde_json::to_value(&settings.quiet_startup).ok() {
            Some(serde_json::Value::Bool(true)) => "true",
            Some(serde_json::Value::String(text)) if text == "header" => "header",
            _ => "false",
        };
        let trust = json(
            serde_json::to_value(settings.default_project_trust).ok(),
            "ask",
        );
        let wheel = match serde_json::to_value(&settings.fullscreen_wheel_scroll_lines).ok() {
            Some(serde_json::Value::Number(number)) => number
                .as_f64()
                .map(|lines| (lines.floor() as i64).clamp(1, 100))
                .map(|lines| lines.to_string()),
            _ => None,
        }
        .unwrap_or_else(|| "auto".to_owned());
        let mut wheel_values: Vec<i64> = vec![1, 2, 3, 5, 10];
        if let Ok(lines) = wheel.parse::<i64>()
            && !wheel_values.contains(&lines)
        {
            wheel_values.push(lines);
            wheel_values.sort_unstable();
        }
        let wheel_values: Vec<String> = std::iter::once("auto".to_owned())
            .chain(wheel_values.iter().map(ToString::to_string))
            .collect();
        let wheel_refs: Vec<&str> = wheel_values.iter().map(String::as_str).collect();
        let timeouts: Vec<&str> = HTTP_IDLE_TIMEOUTS.iter().map(|(label, _)| *label).collect();
        let items = vec![
            item(
                "autocompact",
                "Auto-compact",
                "Automatically compact context when it gets too large",
                flag(config.auto_compact),
                &["true", "false"],
            ),
            item(
                "auto-resize-images",
                "Auto-resize images",
                "Resize large images to 2000x2000 max for better model compatibility",
                flag(images.auto_resize.unwrap_or(true)),
                &["true", "false"],
            ),
            item(
                "block-images",
                "Block images",
                "Prevent images from being sent to LLM providers",
                flag(images.block_images.unwrap_or(false)),
                &["true", "false"],
            ),
            item(
                "skill-commands",
                "Skill commands",
                "Register skills as /skill:name commands",
                flag(settings.enable_skill_commands.unwrap_or(true)),
                &["true", "false"],
            ),
            item(
                "show-hardware-cursor",
                "Show hardware cursor",
                "Show the terminal cursor while still positioning it for IME support",
                flag(
                    settings.show_hardware_cursor.unwrap_or_else(|| {
                        std::env::var("PI_HARDWARE_CURSOR").as_deref() == Ok("1")
                    }),
                ),
                &["true", "false"],
            ),
            item(
                "editor-padding",
                "Editor padding",
                "Horizontal padding for input editor (0-3)",
                settings.editor_padding_x.unwrap_or(0).to_string(),
                &["0", "1", "2", "3"],
            ),
            item(
                "output-padding",
                "Output padding",
                "Horizontal padding for user messages, assistant messages, and thinking",
                if settings.output_pad == Some(0) {
                    "0"
                } else {
                    "1"
                }
                .to_owned(),
                &["0", "1"],
            ),
            item(
                "autocomplete-max-visible",
                "Autocomplete max items",
                "Max visible items in autocomplete dropdown (3-20)",
                settings.autocomplete_max_visible.unwrap_or(5).to_string(),
                &["3", "5", "7", "10", "15", "20"],
            ),
            item(
                "clear-on-shrink",
                "Clear on shrink",
                "Clear empty rows when content shrinks (may cause flicker)",
                flag(
                    terminal.clear_on_shrink.unwrap_or_else(|| {
                        std::env::var("PI_CLEAR_ON_SHRINK").as_deref() == Ok("1")
                    }),
                ),
                &["true", "false"],
            ),
            item(
                "terminal-progress",
                "Terminal progress",
                "Show OSC 9;4 progress indicators in the terminal tab bar",
                flag(terminal.show_terminal_progress.unwrap_or(false)),
                &["true", "false"],
            ),
            item(
                "steering-mode",
                "Steering mode",
                "Enter while streaming queues steering messages. 'one-at-a-time': deliver one, wait for response. 'all': deliver all at once.",
                config.steering_mode.clone(),
                &["one-at-a-time", "all"],
            ),
            item(
                "follow-up-mode",
                "Follow-up mode",
                &format!(
                    "{follow_up_key} queues follow-up messages until agent stops. 'one-at-a-time': deliver one, wait for response. 'all': deliver all at once."
                ),
                config.follow_up_mode.clone(),
                &["one-at-a-time", "all"],
            ),
            item(
                "transport",
                "Transport",
                "Preferred transport for providers that support multiple transports",
                json(serde_json::to_value(settings.transport).ok(), "auto"),
                &["sse", "websocket", "websocket-cached", "auto"],
            ),
            item(
                "http-idle-timeout",
                "HTTP idle timeout",
                "Maximum idle gap while waiting for HTTP headers or body chunks. Disable for local models that pause longer than five minutes.",
                format_http_idle_timeout(config.http_idle_timeout_ms),
                &timeouts,
            ),
            item(
                "cache-warming-mode",
                "Cache warming",
                "off; streaming while the agent runs; idle also between runs while continuation stays profitable",
                json(
                    serde_json::to_value(settings.cache_warming).ok(),
                    "streaming",
                ),
                &["off", "streaming", "idle"],
            ),
            item(
                "hide-thinking",
                "Hide thinking",
                "Hide thinking blocks in assistant responses",
                flag(config.hide_thinking),
                &["true", "false"],
            ),
            item(
                "mermaid-rendering",
                "Mermaid diagrams",
                "Render Mermaid code blocks as Unicode diagrams",
                match settings
                    .markdown
                    .as_ref()
                    .and_then(|markdown| serde_json::to_value(markdown.mermaid).ok())
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .as_deref()
                {
                    Some(mode @ ("off" | "final")) => mode.to_owned(),
                    _ => "streaming".to_owned(),
                },
                &["off", "final", "streaming"],
            ),
            item(
                "cache-miss-notices",
                "Cache miss notices",
                "Show transcript notices for cache costs and provider recovery diagnostics",
                flag(settings.show_cache_miss_notices.unwrap_or(false)),
                &["true", "false"],
            ),
            item(
                "collapse-changelog",
                "Collapse changelog",
                "Show condensed changelog after updates",
                flag(settings.collapse_changelog.unwrap_or(false)),
                &["true", "false"],
            ),
            item(
                "quiet-startup",
                "Quiet startup",
                "Disable verbose printing at startup (header: keep only the startup header)",
                quiet.to_owned(),
                &["true", "header", "false"],
            ),
            item(
                "install-telemetry",
                "Install telemetry",
                "Send an anonymous version/update ping after changelog-detected updates",
                flag(settings.enable_install_telemetry.unwrap_or(true)),
                &["true", "false"],
            ),
            item(
                "default-project-trust",
                "Default project trust",
                "Fallback behavior when no extension or saved trust decision decides project trust",
                project_trust_label(&trust).to_owned(),
                &["Ask", "Always trust", "Never trust"],
            ),
            item(
                "double-escape-action",
                "Double-escape action",
                "Action when pressing Escape twice with empty editor",
                json(
                    serde_json::to_value(settings.double_escape_action).ok(),
                    "tree",
                ),
                &["tree", "fork", "none"],
            ),
            item(
                "tree-filter-mode",
                "Tree filter mode",
                "Default filter when opening /tree",
                json(
                    serde_json::to_value(settings.tree_filter_mode).ok(),
                    "default",
                ),
                &["default", "no-tools", "user-only", "labeled-only", "all"],
            ),
            submenu_item(
                "warnings",
                "Warnings",
                "Enable or disable individual warnings",
                "configure".to_owned(),
            ),
            submenu_item(
                "model-thinking",
                "Default thinking level per model",
                &format!(
                    "Override the default thinking level for specific models. {cycle_key} cycles in-session."
                ),
                overrides_summary(overrides.len()),
            ),
            item(
                "tui-mode",
                "TUI mode",
                "Interface layout; regular mode uses the terminal's normal scrollback",
                if config.fullscreen {
                    "fullscreen"
                } else {
                    "regular"
                }
                .to_owned(),
                &["regular", "fullscreen"],
            ),
            item(
                "fullscreen-exit-output",
                "Fullscreen exit output",
                "Print the transcript or only a session resume hint when exiting fullscreen mode",
                json(
                    serde_json::to_value(settings.fullscreen_exit_output).ok(),
                    "transcript",
                ),
                &["transcript", "resume-hint"],
            ),
            item(
                "fullscreen-scrollbar",
                "Fullscreen scrollbar",
                "Scrollbar behavior in fullscreen mode; has no effect in regular mode",
                json(
                    serde_json::to_value(settings.fullscreen_scrollbar).ok(),
                    "auto",
                ),
                &["auto", "always", "hidden"],
            ),
            item(
                "fullscreen-copy-on-select",
                "Fullscreen copy on select",
                "Automatically copy selected text in fullscreen mode; disable to copy selections with Ctrl+X",
                flag(settings.fullscreen_copy_on_select.unwrap_or(true)),
                &["true", "false"],
            ),
            item(
                "fullscreen-wheel-scroll-lines",
                "Fullscreen wheel scrolling",
                "Lines per mouse-wheel event in fullscreen mode; 'auto' speeds up fast wheel spins where the terminal does not",
                wheel,
                &wheel_refs,
            ),
            submenu_item(
                "theme",
                "Theme",
                "Color theme for the interface",
                config.theme.clone(),
            ),
        ];
        let anthropic_extra_usage = settings
            .warnings
            .as_ref()
            .and_then(|warnings| warnings.anthropic_extra_usage)
            .unwrap_or(true);
        SettingsSelector {
            list: SettingsList::new(items, 10, list_theme(ui), true),
            submenu: None,
            config,
            overrides,
            anthropic_extra_usage,
        }
    }

    /// Shows `value` for `id`, as when a change could not apply.
    pub fn update_value(&mut self, id: &str, value: &str) {
        self.list.update_value(id, value);
    }

    /// The rows at `width`.
    pub fn render(&mut self, width: usize, ui: &Ui<'_>) -> Vec<StyledLine> {
        let mut out = vec![ui.border(width)];
        match &mut self.submenu {
            Some(Submenu::Warnings(list)) => out.extend(list.render(width)),
            Some(Submenu::ModelThinking(menu)) => out.extend(menu.menu.render(width, ui)),
            Some(Submenu::Theme(menu)) => out.extend(menu.render(width, ui)),
            None => out.extend(self.list.render(width)),
        }
        out.push(ui.border(width));
        out
    }

    /// Handles a key.
    pub fn handle_input(&mut self, data: &str, ui: &Ui<'_>) -> Outcome {
        match &mut self.submenu {
            Some(Submenu::Warnings(list)) => {
                return match list.handle_input(data, ui.keys) {
                    SettingsEvent::Changed { id, value } => {
                        self.anthropic_extra_usage = value == "true";
                        Outcome::Side(Action::Setting { id, value })
                    }
                    SettingsEvent::Cancelled => {
                        self.submenu = None;
                        Outcome::None
                    }
                    _ => Outcome::None,
                };
            }
            Some(Submenu::ModelThinking(_)) => return self.model_thinking_input(data, ui),
            Some(Submenu::Theme(menu)) => {
                return match menu.handle_input(data, ui) {
                    ThemeEvent::None => Outcome::None,
                    ThemeEvent::Preview(setting) => Outcome::Side(Action::ThemePreview(setting)),
                    ThemeEvent::Done(Some(setting)) => {
                        self.submenu = None;
                        self.list.update_value("theme", &setting);
                        Outcome::Side(Action::Setting {
                            id: "theme".to_owned(),
                            value: setting,
                        })
                    }
                    ThemeEvent::Done(None) => {
                        let original = menu.original.clone();
                        self.submenu = None;
                        Outcome::Side(Action::ThemePreview(original))
                    }
                };
            }
            None => {}
        }
        match self.list.handle_input(data, ui.keys) {
            SettingsEvent::Changed { id, value } => Outcome::Side(Action::Setting { id, value }),
            SettingsEvent::Open(id) => {
                self.open(&id, ui);
                Outcome::None
            }
            SettingsEvent::Cancelled => Outcome::Cancel,
            SettingsEvent::None => Outcome::None,
        }
    }

    fn open(&mut self, id: &str, ui: &Ui<'_>) {
        self.submenu = match id {
            "warnings" => Some(Submenu::Warnings(Box::new(SettingsList::new(
                vec![item(
                    "anthropic-extra-usage",
                    "Anthropic extra usage",
                    "Warn when Anthropic subscription auth may use paid extra usage",
                    flag(self.anthropic_extra_usage),
                    &["true", "false"],
                )],
                1,
                list_theme(ui),
                false,
            )))),
            "model-thinking" => {
                let config = &self.config;
                let current = config
                    .current_model
                    .as_ref()
                    .map(|(provider, id)| format!("{provider}/{id}"));
                let default = match (
                    &config.settings.default_provider,
                    &config.settings.default_model,
                ) {
                    (Some(provider), Some(id)) => Some(format!("{provider}/{id}")),
                    _ => None,
                }
                .filter(|key| config.models.iter().any(|model| model_key(model) == *key));
                let mut menu = ModelThinking {
                    models: config.models.clone(),
                    current,
                    default,
                    overrides: self.overrides.clone(),
                    global: config.default_thinking,
                    model: None,
                    menu: SelectSubmenu::new(
                        String::new(),
                        String::new(),
                        Vec::new(),
                        "",
                        false,
                        SUBMENU_LAYOUT,
                        ui,
                    ),
                };
                menu.menu = menu.models_step(ui);
                Some(Submenu::ModelThinking(Box::new(menu)))
            }
            "theme" => {
                let setting = self.list.value("theme").unwrap_or_default().to_owned();
                Some(Submenu::Theme(Box::new(ThemeMenu::new(
                    &setting,
                    self.config.themes.clone(),
                    self.config.light_terminal,
                    ui,
                ))))
            }
            _ => None,
        };
    }

    fn model_thinking_input(&mut self, data: &str, ui: &Ui<'_>) -> Outcome {
        let Some(Submenu::ModelThinking(menu)) = &mut self.submenu else {
            return Outcome::None;
        };
        match menu.menu.handle_input(data, ui) {
            SubmenuEvent::Selected(value) => match menu.model.take() {
                None => {
                    menu.menu = menu.levels_step(&value, ui);
                    menu.model = Some(value);
                    Outcome::None
                }
                Some(key) => {
                    let Some(model) = menu.models.iter().find(|model| model_key(model) == key)
                    else {
                        menu.menu = menu.models_step(ui);
                        return Outcome::None;
                    };
                    let (provider, id) = (model.provider.clone(), model.id.clone());
                    let level = if value == CLEAR_OVERRIDE {
                        menu.overrides.retain(|(model, _)| *model != key);
                        None
                    } else {
                        let level = ThinkingLevel::parse(&value);
                        if let Some(level) = level {
                            match menu.overrides.iter_mut().find(|(model, _)| *model == key) {
                                Some(entry) => entry.1 = level,
                                None => menu.overrides.push((key.clone(), level)),
                            }
                        }
                        level
                    };
                    self.overrides.clone_from(&menu.overrides);
                    menu.menu = menu.models_step(ui);
                    Outcome::Side(Action::ModelThinking {
                        provider,
                        id,
                        level,
                    })
                }
            },
            SubmenuEvent::Cancelled => {
                if menu.model.take().is_some() {
                    menu.menu = menu.models_step(ui);
                } else {
                    let summary = overrides_summary(menu.overrides.len());
                    self.submenu = None;
                    self.list.update_value("model-thinking", &summary);
                }
                Outcome::None
            }
            SubmenuEvent::Moved(_) | SubmenuEvent::None => Outcome::None,
        }
    }
}

/// pi's `getAvailableThemes`: the system theme first, then the built-in,
/// agent directory and registered themes by name.
fn available_themes(agent_dir: &std::path::Path, files: &super::themes::ThemeFiles) -> Vec<String> {
    let system = yapi_tui::theme::SYSTEM_THEME_NAME;
    let mut names: Vec<String> = vec![system.to_owned(), "dark".to_owned(), "light".to_owned()];
    if let Ok(entries) = std::fs::read_dir(agent_dir.join("themes")) {
        let mut paths: Vec<std::path::PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect();
        paths.sort();
        for path in paths {
            let name = std::fs::read_to_string(&path)
                .ok()
                .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
                .and_then(|json| json["name"].as_str().map(str::to_owned));
            names.extend(name);
        }
    }
    names.extend(files.names().map(str::to_owned));
    let mut seen = std::collections::HashSet::new();
    names.retain(|name| seen.insert(name.clone()));
    names.sort_by(|a, b| {
        (a != system)
            .cmp(&(b != system))
            .then_with(|| a.to_lowercase().cmp(&b.to_lowercase()))
            .then_with(|| a.cmp(b))
    });
    names
}

/// pi's `parseHttpIdleTimeoutMs` of the setting, or its default.
impl super::App {
    /// pi's `showSettingsSelector`.
    pub(super) fn open_settings(&mut self) {
        let settings = self.session.settings();
        let appearance = match self.colors.background {
            Some(background) => {
                yapi_tui::theme::terminal_appearance(background, self.colors.foreground)
            }
            None => yapi_tui::theme::detect_colorfgbg(std::env::var("COLORFGBG").ok().as_deref())
                .unwrap_or(yapi_tui::theme::Appearance::Dark),
        };
        let mode = |mode: yapi_types::settings::QueueMode| {
            serde_json::to_value(mode)
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_default()
        };
        let config = SettingsConfig {
            auto_compact: self.session.auto_compaction_enabled(),
            steering_mode: mode(self.session.steering_mode()),
            follow_up_mode: mode(self.session.follow_up_mode()),
            http_idle_timeout_ms: self.session.http_idle_timeout_ms(),
            hide_thinking: self.hide_thinking,
            fullscreen: self.fullscreen,
            models: self.session.available_models(),
            current_model: self
                .session
                .model()
                .map(|model| (model.provider.clone(), model.id.clone())),
            default_thinking: settings
                .default_thinking_level
                .unwrap_or(yapi_core::model_resolver::DEFAULT_THINKING_LEVEL),
            theme: self
                .theme_override
                .clone()
                .or_else(|| settings.theme.clone())
                .filter(|theme| !theme.is_empty())
                .unwrap_or_else(|| yapi_tui::theme::SYSTEM_THEME_NAME.to_owned()),
            themes: available_themes(&self.agent_dir, &self.theme_files),
            light_terminal: appearance == yapi_tui::theme::Appearance::Light,
            settings,
        };
        let selector = SettingsSelector::new(config, &self.ui());
        self.selector = Some(super::Selector::Settings(Box::new(selector)));
    }

    /// Loads the theme for `setting` and redraws with it.
    pub(super) fn use_theme(&mut self, setting: Option<&str>) {
        let (theme, error) = super::load_theme(
            setting,
            &self.theme_files,
            &self.agent_dir,
            &self.colors,
            self.color_mode,
        );
        self.markdown = super::markdown_theme(&theme);
        self.editor.set_theme(super::editor_theme(&theme));
        self.theme = theme;
        self.style_alt_screen();
        self.invalidate_all();
        if let Some(error) = error {
            self.error(error);
        }
    }

    /// Applies a `/settings` change and saves it, as pi's callbacks do.
    pub(super) fn apply_setting(&mut self, id: &str, value: &str) {
        use serde_json::Value;
        let on = value == "true";
        let number = || value.parse::<u64>().unwrap_or_default();
        let session = self.session.clone();
        let global = |key: &str, value: Value| {
            let _ = session.set_global_setting(key, Some(value));
        };
        let nested = |field: &str, key: &str, value: Value| {
            let _ = session.set_nested_global_setting(field, key, value);
        };
        let text = || Value::String(value.to_owned());
        match id {
            "autocompact" => {
                let _ = self.session.set_auto_compaction(on);
                self.footer_cache = None;
            }
            "auto-resize-images" => nested("images", "autoResize", on.into()),
            "block-images" => nested("images", "blockImages", on.into()),
            "skill-commands" => {
                global("enableSkillCommands", on.into());
                self.install_autocomplete();
            }
            "show-hardware-cursor" => {
                global("showHardwareCursor", on.into());
                self.main.show_hardware_cursor = on;
                self.alt.show_hardware_cursor = on;
            }
            "editor-padding" => {
                global("editorPaddingX", number().min(3).into());
                self.editor.set_padding_x(number().min(3) as usize);
            }
            "output-padding" => {
                global("outputPad", number().min(1).into());
                self.output_pad = number().min(1) as usize;
                self.invalidate_all();
            }
            "autocomplete-max-visible" => {
                global("autocompleteMaxVisible", number().clamp(3, 20).into());
                self.editor.set_autocomplete_max_visible(number() as usize);
            }
            "clear-on-shrink" => {
                nested("terminal", "clearOnShrink", on.into());
                self.main.clear_on_shrink = on;
            }
            "terminal-progress" => nested("terminal", "showTerminalProgress", on.into()),
            "steering-mode" | "follow-up-mode" => {
                if let Ok(mode) = serde_json::from_value(text()) {
                    let _ = if id == "steering-mode" {
                        self.session.set_steering_mode(mode)
                    } else {
                        self.session.set_follow_up_mode(mode)
                    };
                }
            }
            "transport" => global("transport", text()),
            "http-idle-timeout" => {
                if let Some((label, ms)) =
                    HTTP_IDLE_TIMEOUTS.iter().find(|(label, _)| *label == value)
                {
                    global("httpIdleTimeoutMs", (*ms).into());
                    yapi_ai::http::set_idle_timeout_ms(*ms);
                    self.status(format!("HTTP idle timeout: {label}"));
                }
            }
            "cache-warming-mode" => {
                global("cacheWarming", text());
                self.status(format!("Cache warming: {value}"));
            }
            "hide-thinking" => {
                self.hide_thinking = on;
                global("hideThinkingBlock", on.into());
                self.invalidate_all();
            }
            "mermaid-rendering" => {
                nested("markdown", "mermaid", text());
                self.invalidate_all();
            }
            "cache-miss-notices" => {
                global("showCacheMissNotices", on.into());
                self.invalidate_all();
            }
            "collapse-changelog" => global("collapseChangelog", on.into()),
            "quiet-startup" => global(
                "quietStartup",
                if value == "header" { text() } else { on.into() },
            ),
            "install-telemetry" => global("enableInstallTelemetry", on.into()),
            "default-project-trust" => {
                let trust = match value {
                    "Always trust" => "always",
                    "Never trust" => "never",
                    _ => "ask",
                };
                global("defaultProjectTrust", trust.into());
            }
            "double-escape-action" => global("doubleEscapeAction", text()),
            "tree-filter-mode" => global("treeFilterMode", text()),
            "anthropic-extra-usage" => nested("warnings", "anthropicExtraUsage", on.into()),
            "tui-mode" => {
                let fullscreen = value == "fullscreen";
                if self.overlay.is_some() {
                    let current = if self.fullscreen {
                        "fullscreen"
                    } else {
                        "regular"
                    };
                    if let Some(super::Selector::Settings(selector)) = &mut self.selector {
                        selector.update_value("tui-mode", current);
                    }
                    self.status("Close active overlays before changing TUI mode");
                    return;
                }
                self.switch_tui_mode(fullscreen);
                global("tuiMode", text());
                self.status(format!("TUI mode: {value}"));
            }
            "fullscreen-exit-output" => global("fullscreenExitOutput", text()),
            "fullscreen-scrollbar" => {
                global("fullscreenScrollbar", text());
                self.style_alt_screen();
                self.invalidate_all();
            }
            "fullscreen-copy-on-select" => global("fullscreenCopyOnSelect", on.into()),
            "fullscreen-wheel-scroll-lines" => global(
                "fullscreenWheelScrollLines",
                if value == "auto" {
                    text()
                } else {
                    number().into()
                },
            ),
            "theme" => {
                global("theme", text());
                self.theme_override = None;
                self.use_theme(Some(value));
            }
            _ => {}
        }
    }

    /// Sets or clears the default thinking level of a model, applying it
    /// when that model is in use.
    pub(super) fn set_model_thinking(
        &mut self,
        provider: &str,
        id: &str,
        level: Option<ThinkingLevel>,
    ) {
        let key = format!("{provider}/{id}");
        let mut levels = self
            .session
            .settings()
            .model_thinking_levels
            .unwrap_or_default();
        match level {
            Some(level) => {
                levels.insert(key, level);
            }
            None => {
                levels.shift_remove(&key);
            }
        }
        let value = (!levels.is_empty())
            .then(|| serde_json::to_value(&levels).ok())
            .flatten();
        let _ = self
            .session
            .set_global_setting("modelThinkingLevels", value);
        let current = self
            .session
            .model()
            .is_some_and(|model| model.provider == provider && model.id == id);
        if current {
            let level = level.unwrap_or_else(|| {
                self.session
                    .settings()
                    .default_thinking_level
                    .unwrap_or(yapi_core::model_resolver::DEFAULT_THINKING_LEVEL)
            });
            self.session.set_thinking_level(level);
            self.footer_cache = None;
        }
    }
}
