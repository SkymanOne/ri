//! Components that take the editor's place: the model, thinking and fork
//! selectors, and the generic choice and text dialogs.
//!
//! Ports of `model-selector.ts`, `thinking-selector.ts`,
//! `user-message-selector.ts`, `extension-selector.ts` and
//! `extension-editor.ts` in
//! `packages/coding-agent/src/modes/interactive/components` in pi `v1.0.0`.

use std::path::PathBuf;
use std::time::Instant;

use ratatui_core::style::{Modifier, Style};
use ratatui_core::text::{Line, Span};
use ri_tui::color::ColorMode;
use ri_tui::editor::{Editor, EditorEvent, EditorTheme};
use ri_tui::fuzzy::fuzzy_filter;
use ri_tui::keybindings::Keybindings;
use ri_tui::lines::{self, StyledLine, styled};
use ri_tui::select_list::{SelectEvent, SelectItem, SelectList, SelectListLayout, SelectListTheme};
use ri_tui::text::truncate_to_width;
use ri_tui::text_input::{InputEvent, TextInput};
use ri_tui::theme::Theme;
use ri_types::message::ThinkingLevel;
use ri_types::model::Model;

use super::catalogs::RefreshStatus;
use super::keybindings::{keys_display, keys_text};
use super::session_selector::SessionSelector;
use super::tree_selector::TreeSelector;

/// What a selector asks the app to do.
#[derive(Clone, Debug)]
pub enum Action {
    /// Use a model; `default` also saves it to settings.
    Model { model: Box<Model>, default: bool },
    /// Use a thinking level; `default` also saves it to settings.
    Thinking { level: ThinkingLevel, default: bool },
    /// Fork from a user message entry.
    Fork(String),
    /// A choice dialog's option, by index.
    Choice(usize),
    /// A text dialog's value.
    Text(String),
    /// Resume a session file.
    Resume(PathBuf),
    /// Navigate the tree to an entry.
    Tree(String),
    /// Set or clear an entry's label.
    Label { id: String, label: Option<String> },
    /// Copy text; `None` when the entry has none.
    Copy(Option<String>),
    /// Toggle tool output expansion.
    ToggleTools,
    /// A provider chosen for `/login` or `/logout`.
    Provider(Box<super::login::ProviderOption>),
    /// The login dialog was closed with escape.
    LoginCancelled,
    /// A project trust decision to save.
    Trust(Box<ri_core::trust::TrustOption>),
    /// A `/settings` change: the setting's id and its new value as shown.
    Setting {
        /// The setting.
        id: String,
        /// Its new value.
        value: String,
    },
    /// Show a theme setting without saving it.
    ThemePreview(String),
    /// The models `ctrl+p` cycles through; `None` is every model. `save`
    /// also writes them to settings.
    ScopedModels {
        /// Enabled model ids in order.
        enabled: Option<Vec<String>>,
        /// Save to settings instead of changing the session.
        save: bool,
    },
    /// Set or clear a model's default thinking level.
    ModelThinking {
        /// The model's provider.
        provider: String,
        /// The model's id.
        id: String,
        /// The level; `None` clears the override.
        level: Option<ThinkingLevel>,
    },
}

/// The result of a key.
#[derive(Clone, Debug)]
pub enum Outcome {
    /// Nothing for the app to do.
    None,
    /// Close without acting.
    Cancel,
    /// Close, then act.
    Done(Action),
    /// Act and stay open.
    Side(Action),
}

/// Styles and bindings selectors render and decode with.
pub struct Ui<'a> {
    /// The theme.
    pub theme: &'a Theme,
    /// Key bindings.
    pub keys: &'a Keybindings,
}

impl Ui<'_> {
    /// pi's `keyHint`: the action's keys, dim, then the description, muted.
    pub fn key_hint(&self, action: &str, description: &str) -> Vec<Span<'static>> {
        vec![
            Span::styled(keys_text(self.keys, action), self.theme.fg("dim")),
            Span::styled(format!(" {description}"), self.theme.fg("muted")),
        ]
    }

    /// pi's `rawKeyHint`.
    pub fn raw_key_hint(&self, key: &str, description: &str) -> Vec<Span<'static>> {
        vec![
            Span::styled(key.to_owned(), self.theme.fg("dim")),
            Span::styled(format!(" {description}"), self.theme.fg("muted")),
        ]
    }

    /// pi's `getSelectListTheme`.
    pub fn select_list_theme(&self) -> SelectListTheme {
        SelectListTheme {
            selected_text: self.theme.fg("accent"),
            description: self.theme.fg("muted"),
            scroll_info: self.theme.fg("muted"),
            no_match: self.theme.fg("muted"),
        }
    }

    /// pi's default `DynamicBorder`.
    pub fn border(&self, width: usize) -> StyledLine {
        lines::border(width, self.theme.fg("border"))
    }
}

/// One-line `Text` rows with padding 0.
fn text_row(line: StyledLine, width: usize) -> Vec<StyledLine> {
    lines::text(&[line], width, 0, 0, None)
}

/// A selector in the editor's place.
pub enum Selector {
    /// `/model`.
    Model(Box<ModelSelector>),
    /// `/thinking`.
    Thinking(Box<ThinkingSelector>),
    /// `/fork`.
    Fork(ForkSelector),
    /// A choice among options.
    Choice(ChoiceDialog),
    /// A multi-line text dialog.
    Text(Box<TextDialog>),
    /// `/resume`.
    Session(Box<SessionSelector>),
    /// `/tree`.
    Tree(Box<TreeSelector>),
    /// `/login` and `/logout` providers.
    Providers(Box<super::login::ProviderSelector>),
    /// A sign-in in progress.
    Login(Box<super::login::LoginDialog>),
    /// A line of text for an extension.
    Input(Box<InputDialog>),
    /// An extension's component.
    Remote(Box<super::extension_ui::RemoteView>),
    /// `/trust`.
    Trust(Box<TrustSelector>),
    /// `/settings`.
    Settings(Box<super::settings_selector::SettingsSelector>),
    /// `/scoped-models`.
    ScopedModels(Box<super::scoped_models::ScopedModelsSelector>),
    /// `ri config`.
    Config(Box<super::config_selector::ConfigSelector>),
}

impl Selector {
    /// The rows at `width`, and the terminal cursor among them.
    pub fn render(
        &mut self,
        width: usize,
        ui: &Ui<'_>,
    ) -> (Vec<StyledLine>, Option<(usize, usize)>) {
        match self {
            Selector::Model(selector) => selector.render(width, ui),
            Selector::Thinking(selector) => selector.render(width, ui),
            Selector::Fork(selector) => (selector.render(width, ui), None),
            Selector::Choice(dialog) => (dialog.render(width, ui), None),
            Selector::Text(dialog) => dialog.render(width, ui),
            Selector::Session(selector) => selector.render(width, ui),
            Selector::Tree(selector) => selector.render(width, ui),
            Selector::Providers(selector) => selector.render(width, ui),
            Selector::Login(dialog) => dialog.render(width, ui),
            Selector::Input(dialog) => dialog.render(width, ui),
            Selector::Remote(view) => view.render(width),
            Selector::Trust(selector) => (selector.render(width, ui), None),
            Selector::Settings(selector) => (selector.render(width, ui), None),
            Selector::ScopedModels(selector) => selector.render(width, ui),
            Selector::Config(selector) => (selector.render(width, ui), None),
        }
    }

    /// Handles a key.
    pub fn handle_input(&mut self, data: &str, ui: &Ui<'_>) -> Outcome {
        match self {
            Selector::Model(selector) => selector.handle_input(data, ui),
            Selector::Thinking(selector) => selector.handle_input(data, ui),
            Selector::Fork(selector) => selector.handle_input(data, ui),
            Selector::Choice(dialog) => dialog.handle_input(data, ui),
            Selector::Text(dialog) => dialog.handle_input(data, ui),
            Selector::Session(selector) => selector.handle_input(data, ui),
            Selector::Tree(selector) => selector.handle_input(data, ui),
            Selector::Providers(selector) => selector.handle_input(data, ui),
            Selector::Login(dialog) => dialog.handle_input(data, ui),
            Selector::Input(dialog) => dialog.handle_input(data, ui),
            Selector::Remote(view) => {
                view.input(data);
                Outcome::None
            }
            Selector::Trust(selector) => selector.handle_input(data, ui),
            Selector::Settings(selector) => selector.handle_input(data, ui),
            Selector::ScopedModels(selector) => selector.handle_input(data, ui),
            Selector::Config(selector) => selector.handle_input(data, ui),
        }
    }

    /// Called on every frame; selectors with timed state update it.
    pub fn tick(&mut self) {
        if let Selector::Session(selector) = self {
            selector.tick();
        }
    }
}

fn same_model(a: Option<&Model>, b: &Model) -> bool {
    a.is_some_and(|a| a.provider == b.provider && a.id == b.id)
}

/// pi's `getModelSelectorSearchText`.
fn model_search_text(model: &Model) -> String {
    let name = if model.name.is_empty() {
        String::new()
    } else {
        format!(" {}", model.name)
    };
    format!("{0} {0}/{1} {0} {1}{name}", model.provider, model.id)
}

/// The `/model` selector.
pub struct ModelSelector {
    input: TextInput,
    /// The models of the current scope.
    models: Vec<Model>,
    all: Vec<Model>,
    /// The session's scope, in its order; empty when it has none.
    scoped: Vec<Model>,
    in_scope: bool,
    filtered: Vec<usize>,
    selected: usize,
    current: Option<Model>,
    default: Option<(String, String)>,
    /// The catalog refresh this selector waits on.
    pub refresh_id: u64,
    refresh: RefreshStatus,
}

fn sort_models(models: &mut [Model], current: Option<&Model>, default: Option<&(String, String)>) {
    let is_default = |model: &Model| {
        default.is_some_and(|(provider, id)| *provider == model.provider && *id == model.id)
    };
    models.sort_by(|a, b| {
        let a_current = same_model(current, a);
        let b_current = same_model(current, b);
        b_current
            .cmp(&a_current)
            .then_with(|| is_default(b).cmp(&is_default(a)))
            .then_with(|| ri_types::collate::locale_compare(&a.provider, &b.provider))
    });
}

impl ModelSelector {
    /// A selector over `models`, the current one first, then the default,
    /// showing the session's `scoped` models first when there are any.
    pub fn new(
        mut models: Vec<Model>,
        scoped: Vec<Model>,
        current: Option<Model>,
        default: Option<(String, String)>,
        search: &str,
    ) -> ModelSelector {
        sort_models(&mut models, current.as_ref(), default.as_ref());
        let mut input = TextInput::default();
        input.focused = true;
        input.set_value(search);
        let in_scope = !scoped.is_empty();
        let active = if in_scope {
            scoped.clone()
        } else {
            models.clone()
        };
        let mut selector = ModelSelector {
            input,
            filtered: (0..active.len()).collect(),
            models: active,
            all: models,
            scoped,
            in_scope,
            selected: 0,
            current,
            default,
            refresh_id: 0,
            refresh: RefreshStatus::Running,
        };
        selector.selected = selector
            .filtered
            .iter()
            .position(|&index| same_model(selector.current.as_ref(), &selector.models[index]))
            .unwrap_or(0);
        if !search.is_empty() {
            selector.filter();
        }
        selector
    }

    /// The catalog refresh finished: pi's `loadModelsFromSnapshot` with the
    /// refreshed `models`, then the search again.
    pub fn refreshed(&mut self, mut models: Vec<Model>, status: RefreshStatus) {
        sort_models(&mut models, self.current.as_ref(), self.default.as_ref());
        for scoped in &mut self.scoped {
            if let Some(model) = models.iter().find(|model| same_model(Some(scoped), model)) {
                *scoped = model.clone();
            }
        }
        self.all = models;
        self.models = if self.in_scope {
            self.scoped.clone()
        } else {
            self.all.clone()
        };
        self.selected = self
            .models
            .iter()
            .position(|model| same_model(self.current.as_ref(), model))
            .unwrap_or_else(|| self.selected.min(self.models.len().saturating_sub(1)));
        self.refresh = status;
        self.filter();
    }

    /// pi's `setScope`: the other scope's models, the current one selected.
    fn toggle_scope(&mut self) {
        self.in_scope = !self.in_scope;
        self.models = if self.in_scope {
            self.scoped.clone()
        } else {
            self.all.clone()
        };
        self.selected = self
            .models
            .iter()
            .position(|model| same_model(self.current.as_ref(), model))
            .unwrap_or(0);
        self.filter();
    }

    fn is_default(&self, model: &Model) -> bool {
        self.default
            .as_ref()
            .is_some_and(|(provider, id)| *provider == model.provider && *id == model.id)
    }

    fn filter(&mut self) {
        let query = self.input.value().to_owned();
        if query.is_empty() {
            self.filtered = (0..self.models.len()).collect();
            self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
            return;
        }
        let indexes: Vec<usize> = (0..self.models.len()).collect();
        let filtered = fuzzy_filter(indexes, &query, |&index| {
            let model = &self.models[index];
            let default = if self.is_default(model) {
                " default"
            } else {
                ""
            };
            format!("{}{default}", model_search_text(model))
        });
        let normalized = query.trim().to_lowercase();
        if !normalized.is_empty() && "default".starts_with(&normalized) {
            let defaults: Vec<usize> = (0..self.models.len())
                .filter(|&index| self.is_default(&self.models[index]))
                .collect();
            self.filtered = defaults
                .iter()
                .copied()
                .chain(
                    filtered
                        .into_iter()
                        .filter(|index| !defaults.contains(index)),
                )
                .collect();
        } else {
            self.filtered = filtered;
        }
        self.selected = 0;
    }

    fn render(&mut self, width: usize, ui: &Ui<'_>) -> (Vec<StyledLine>, Option<(usize, usize)>) {
        let theme = ui.theme;
        let mut out = vec![ui.border(width)];
        out.extend(lines::spacer(1));
        if self.scoped.is_empty() {
            out.extend(text_row(
                styled(
                    "Only showing models from configured providers. Use /login to add providers.",
                    theme.fg("warning"),
                ),
                width,
            ));
        } else {
            let muted = theme.fg("muted");
            let pick = |on: bool| if on { theme.fg("accent") } else { muted };
            out.extend(text_row(
                Line::from(vec![
                    Span::styled("Scope: ", muted),
                    Span::styled("all", pick(!self.in_scope)),
                    Span::styled(" | ", muted),
                    Span::styled("scoped", pick(self.in_scope)),
                ]),
                width,
            ));
            let mut hint = ui.key_hint("tui.input.tab", "scope");
            hint.push(Span::styled(" (all/scoped)", muted));
            out.extend(text_row(Line::from(hint), width));
        }
        out.extend(lines::spacer(1));
        let input = self.input.render(width);
        let cursor = self.input.cursor_column().map(|col| (out.len(), col));
        out.push(input);
        out.extend(lines::spacer(1));
        let max_visible = 10usize;
        let count = self.filtered.len();
        let start = self
            .selected
            .saturating_sub(max_visible / 2)
            .min(count.saturating_sub(max_visible));
        let end = (start + max_visible).min(count);
        for position in start..end {
            let model = &self.models[self.filtered[position]];
            let selected = position == self.selected;
            let mut spans = vec![
                if selected {
                    Span::styled("→ ", theme.fg("accent"))
                } else {
                    Span::raw("  ")
                },
                if same_model(self.current.as_ref(), model) {
                    Span::styled("✓ ", theme.fg("accent"))
                } else {
                    Span::raw("  ")
                },
                if selected {
                    Span::styled(model.id.clone(), theme.fg("accent"))
                } else {
                    Span::raw(model.id.clone())
                },
                Span::raw(" "),
                Span::styled(format!("[{}]", model.provider), theme.fg("muted")),
            ];
            if self.is_default(model) {
                spans.push(Span::styled(" · default", theme.fg("muted")));
            }
            out.extend(text_row(Line::from(spans), width));
        }
        if start > 0 || end < count {
            out.extend(text_row(
                styled(
                    format!("  ({}/{})", self.selected + 1, count),
                    theme.fg("muted"),
                ),
                width,
            ));
        }
        match (&self.refresh, self.filtered.get(self.selected)) {
            (RefreshStatus::Failed(error), _) => {
                for line in error.split('\n') {
                    out.extend(text_row(styled(line, theme.fg("error")), width));
                }
            }
            (_, None) => out.extend(text_row(
                styled("  No matching models", theme.fg("muted")),
                width,
            )),
            (_, Some(&index)) => {
                out.extend(lines::spacer(1));
                out.extend(text_row(
                    styled(
                        format!("  Model Name: {}", self.models[index].name),
                        theme.fg("muted"),
                    ),
                    width,
                ));
            }
        }
        let status = match self.refresh {
            RefreshStatus::Running => Some((RefreshStatus::RUNNING, "muted")),
            RefreshStatus::Done => Some((RefreshStatus::DONE, "success")),
            RefreshStatus::Failed(_) => None,
        };
        if let Some((text, color)) = status {
            out.extend(lines::spacer(1));
            out.extend(text_row(
                styled(format!("  {text}"), theme.fg(color)),
                width,
            ));
        }
        out.extend(lines::spacer(1));
        out.extend(text_row(
            styled(
                format!(
                    "  {} to select · {} to set as default · {} to cancel",
                    keys_display(ui.keys, "tui.select.confirm"),
                    keys_display(ui.keys, "app.models.save"),
                    keys_display(ui.keys, "tui.select.cancel")
                ),
                theme.fg("dim"),
            ),
            width,
        ));
        out.push(ui.border(width));
        (out, cursor)
    }

    fn chosen(&self, default: bool) -> Outcome {
        match self.filtered.get(self.selected) {
            Some(&index) => Outcome::Done(Action::Model {
                model: Box::new(self.models[index].clone()),
                default,
            }),
            None => Outcome::None,
        }
    }

    fn handle_input(&mut self, data: &str, ui: &Ui<'_>) -> Outcome {
        let kb = ui.keys;
        let count = self.filtered.len();
        if kb.matches(data, "tui.input.tab") {
            if !self.scoped.is_empty() {
                self.toggle_scope();
            }
            return Outcome::None;
        }
        if kb.matches(data, "tui.select.up") {
            if count > 0 {
                self.selected = if self.selected == 0 {
                    count - 1
                } else {
                    self.selected - 1
                };
            }
        } else if kb.matches(data, "tui.select.down") {
            if count > 0 {
                self.selected = if self.selected + 1 >= count {
                    0
                } else {
                    self.selected + 1
                };
            }
        } else if kb.matches(data, "tui.select.confirm") {
            return self.chosen(false);
        } else if kb.matches(data, "tui.select.cancel") {
            return Outcome::Cancel;
        } else if kb.matches(data, "app.models.save") {
            return self.chosen(true);
        } else {
            if let InputEvent::Submit(_) = self.input.handle_input(data, kb) {
                return self.chosen(false);
            }
            self.filter();
        }
        Outcome::None
    }
}

/// pi's descriptions of thinking levels.
pub fn level_description(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Off => "No reasoning",
        ThinkingLevel::Minimal => "Very brief reasoning (~1k tokens)",
        ThinkingLevel::Low => "Light reasoning (~2k tokens)",
        ThinkingLevel::Medium => "Moderate reasoning (~8k tokens)",
        ThinkingLevel::High => "Deep reasoning (~16k tokens)",
        ThinkingLevel::Xhigh => "Extra-high reasoning (~32k tokens)",
        ThinkingLevel::Max => "Maximum reasoning",
    }
}

const PRIMARY_LAYOUT: SelectListLayout = SelectListLayout {
    min_primary_column_width: Some(12),
    max_primary_column_width: Some(32),
};

/// The `/thinking` selector.
pub struct ThinkingSelector {
    input: TextInput,
    items: Vec<SelectItem>,
    list: SelectList,
    theme: SelectListTheme,
}

impl ThinkingSelector {
    /// A selector over `levels` with `current` highlighted.
    pub fn new(
        current: ThinkingLevel,
        levels: &[ThinkingLevel],
        default: Option<ThinkingLevel>,
        theme: SelectListTheme,
    ) -> ThinkingSelector {
        let items: Vec<SelectItem> = levels
            .iter()
            .map(|level| SelectItem {
                value: level.as_str().to_owned(),
                label: format!(
                    "{}{}",
                    if *level == current { "✓ " } else { "  " },
                    level.as_str()
                ),
                description: Some(if Some(*level) == default {
                    format!("{} · default", level_description(*level))
                } else {
                    level_description(*level).to_owned()
                }),
            })
            .collect();
        let mut input = TextInput::default();
        input.focused = true;
        let list = Self::list(items.clone(), Some(current.as_str()), theme);
        ThinkingSelector {
            input,
            items,
            list,
            theme,
        }
    }

    fn list(items: Vec<SelectItem>, selected: Option<&str>, theme: SelectListTheme) -> SelectList {
        let index = selected.and_then(|value| items.iter().position(|item| item.value == value));
        let mut list = SelectList::new(items.clone(), items.len().max(1), theme, PRIMARY_LAYOUT);
        if let Some(index) = index {
            list.set_selected_index(index);
        }
        list
    }

    fn render(&mut self, width: usize, ui: &Ui<'_>) -> (Vec<StyledLine>, Option<(usize, usize)>) {
        let theme = ui.theme;
        let mut out = vec![ui.border(width)];
        out.extend(lines::spacer(1));
        out.extend(text_row(lines::raw("Thinking Level"), width));
        out.extend(lines::spacer(1));
        out.extend(text_row(
            lines::raw(format!(
                "{} cycles thinking levels in-session",
                keys_display(ui.keys, "app.thinking.cycle")
            )),
            width,
        ));
        out.extend(lines::spacer(1));
        let input = self.input.render(width);
        let cursor = self.input.cursor_column().map(|col| (out.len(), col));
        out.push(input);
        out.extend(lines::spacer(1));
        out.extend(self.list.render(width));
        out.extend(lines::spacer(1));
        out.extend(text_row(
            styled(
                format!(
                    "  {} to select · {} to set as default · {} to cancel",
                    keys_display(ui.keys, "tui.select.confirm"),
                    keys_display(ui.keys, "app.thinking.save"),
                    keys_display(ui.keys, "tui.select.cancel")
                ),
                theme.fg("dim"),
            ),
            width,
        ));
        out.push(ui.border(width));
        (out, cursor)
    }

    fn selected_level(&self) -> Option<ThinkingLevel> {
        self.list
            .selected_item()
            .and_then(|item| ThinkingLevel::parse(&item.value))
    }

    fn handle_input(&mut self, data: &str, ui: &Ui<'_>) -> Outcome {
        let kb = ui.keys;
        if kb.matches(data, "app.thinking.save") {
            return match self.selected_level() {
                Some(level) => Outcome::Done(Action::Thinking {
                    level,
                    default: true,
                }),
                None => Outcome::None,
            };
        }
        let navigation = [
            "tui.select.up",
            "tui.select.down",
            "tui.select.confirm",
            "tui.select.cancel",
        ]
        .iter()
        .any(|action| kb.matches(data, action));
        if navigation {
            return match self.list.handle_input(data, kb) {
                SelectEvent::Selected(item) => match ThinkingLevel::parse(&item.value) {
                    Some(level) => Outcome::Done(Action::Thinking {
                        level,
                        default: false,
                    }),
                    None => Outcome::None,
                },
                SelectEvent::Cancelled => Outcome::Cancel,
                _ => Outcome::None,
            };
        }
        if let InputEvent::Submit(_) = self.input.handle_input(data, kb) {
            return match self.list.handle_input("\r", kb) {
                SelectEvent::Selected(item) => match ThinkingLevel::parse(&item.value) {
                    Some(level) => Outcome::Done(Action::Thinking {
                        level,
                        default: false,
                    }),
                    None => Outcome::None,
                },
                _ => Outcome::None,
            };
        }
        let query = self.input.value().to_owned();
        let filtered = if query.is_empty() {
            self.items.clone()
        } else {
            fuzzy_filter(self.items.clone(), &query, |item| {
                format!(
                    "{} {}",
                    item.value,
                    item.description.as_deref().unwrap_or_default()
                )
            })
        };
        let selected = self.list.selected_item().map(|item| item.value.clone());
        self.list = Self::list(filtered, selected.as_deref(), self.theme);
        Outcome::None
    }
}

/// The `/fork` selector over user messages.
pub struct ForkSelector {
    messages: Vec<(String, String)>,
    selected: usize,
}

impl ForkSelector {
    /// A selector over `(entry id, text)` pairs, the last one highlighted.
    pub fn new(messages: Vec<(String, String)>) -> ForkSelector {
        let selected = messages.len().saturating_sub(1);
        ForkSelector { messages, selected }
    }

    fn render(&self, width: usize, ui: &Ui<'_>) -> Vec<StyledLine> {
        let theme = ui.theme;
        let mut out = lines::spacer(1);
        out.extend(lines::text(
            &[styled(
                "Fork from Message",
                Style::new().add_modifier(Modifier::BOLD),
            )],
            width,
            1,
            0,
            None,
        ));
        out.extend(lines::text(
            &[styled(
                "Select a user message to copy the active path up to that point into a new session",
                theme.fg("muted"),
            )],
            width,
            1,
            0,
            None,
        ));
        out.extend(lines::spacer(1));
        out.push(ui.border(width));
        out.extend(lines::spacer(1));
        let count = self.messages.len();
        if count == 0 {
            out.push(styled("  No user messages found", theme.fg("muted")));
        } else {
            let max_visible = 10usize;
            let start = self
                .selected
                .saturating_sub(max_visible / 2)
                .min(count.saturating_sub(max_visible));
            let end = (start + max_visible).min(count);
            for position in start..end {
                let selected = position == self.selected;
                let text = self.messages[position].1.replace('\n', " ");
                let text = truncate_to_width(text.trim(), width.saturating_sub(2), "...", false);
                out.push(Line::from(vec![
                    if selected {
                        Span::styled("› ", theme.fg("accent"))
                    } else {
                        Span::raw("  ")
                    },
                    if selected {
                        Span::styled(text, Style::new().add_modifier(Modifier::BOLD))
                    } else {
                        Span::raw(text)
                    },
                ]));
                out.push(styled(
                    format!("  Message {} of {count}", position + 1),
                    theme.fg("muted"),
                ));
                out.push(Line::default());
            }
            if start > 0 || end < count {
                out.push(styled(
                    format!("  ({}/{count})", self.selected + 1),
                    theme.fg("muted"),
                ));
            }
        }
        out.extend(lines::spacer(1));
        out.push(ui.border(width));
        out
    }

    fn handle_input(&mut self, data: &str, ui: &Ui<'_>) -> Outcome {
        let kb = ui.keys;
        let count = self.messages.len();
        if kb.matches(data, "tui.select.up") {
            self.selected = if self.selected == 0 {
                count.saturating_sub(1)
            } else {
                self.selected - 1
            };
        } else if kb.matches(data, "tui.select.down") {
            self.selected = if self.selected + 1 >= count {
                0
            } else {
                self.selected + 1
            };
        } else if kb.matches(data, "tui.select.confirm") {
            if let Some((id, _)) = self.messages.get(self.selected) {
                return Outcome::Done(Action::Fork(id.clone()));
            }
        } else if kb.matches(data, "tui.select.cancel") {
            return Outcome::Cancel;
        }
        Outcome::None
    }
}

/// pi's `CountdownTimer` as a title suffix: whole seconds left, rounded up,
/// until the dialog closes on its own.
#[derive(Clone, Copy, Debug)]
pub struct Countdown(pub std::time::Instant);

impl Countdown {
    /// `title` with the seconds left.
    fn title(self, title: &str) -> String {
        let left = self
            .0
            .saturating_duration_since(std::time::Instant::now())
            .as_millis();
        format!("{title} ({}s)", left.div_ceil(1000))
    }
}

/// pi's `TrustSelectorComponent`: saves whether to trust the project.
pub struct TrustSelector {
    cwd: String,
    options: Vec<ri_core::trust::TrustOption>,
    saved: Option<(PathBuf, bool)>,
    trusted: bool,
    selected: usize,
}

impl TrustSelector {
    /// The selector for `cwd`, with its stored decision and whether this
    /// session trusts it; the stored decision's option starts selected.
    pub fn new(
        cwd: &std::path::Path,
        saved: Option<(PathBuf, bool)>,
        trusted: bool,
    ) -> TrustSelector {
        let options = ri_core::trust::trust_options(cwd, false);
        let mut selector = TrustSelector {
            cwd: cwd.display().to_string(),
            options,
            saved,
            trusted,
            selected: 0,
        };
        selector.selected = (0..selector.options.len())
            .find(|&index| selector.is_saved(index))
            .unwrap_or(0);
        selector
    }

    fn is_saved(&self, index: usize) -> bool {
        let option = &self.options[index];
        match (&self.saved, &option.saved_path) {
            (Some((path, decision)), Some(saved_path)) => {
                *decision == option.trusted && path == saved_path
            }
            _ => false,
        }
    }

    fn render(&self, width: usize, ui: &Ui<'_>) -> Vec<StyledLine> {
        let theme = ui.theme;
        let muted =
            |text: String| lines::text(&[styled(text, theme.fg("muted"))], width, 1, 0, None);
        let mut out = vec![ui.border(width)];
        out.extend(lines::spacer(1));
        out.extend(lines::text(
            &[styled(
                "Project trust",
                theme.fg("accent").add_modifier(Modifier::BOLD),
            )],
            width,
            1,
            0,
            None,
        ));
        out.extend(muted(self.cwd.clone()));
        out.extend(lines::spacer(1));
        let decision = match &self.saved {
            None => "none".to_owned(),
            Some((path, decision)) => {
                let label = if *decision { "trusted" } else { "untrusted" };
                let own = self
                    .options
                    .first()
                    .and_then(|option| option.saved_path.as_ref());
                if own.is_some_and(|own| own != path) {
                    format!("{label} (inherited from {})", path.display())
                } else {
                    format!("{label} ({})", path.display())
                }
            }
        };
        out.extend(muted(format!("Saved decision: {decision}")));
        out.extend(muted(format!(
            "Current session: {}",
            if self.trusted { "trusted" } else { "untrusted" }
        )));
        out.extend(lines::spacer(1));
        for (index, option) in self.options.iter().enumerate() {
            let selected = index == self.selected;
            let mut spans = vec![if selected {
                Span::styled("→ ", theme.fg("accent"))
            } else {
                Span::raw("  ")
            }];
            spans.push(if self.is_saved(index) {
                Span::styled("✓ ", theme.fg("accent"))
            } else {
                Span::raw("  ")
            });
            spans.push(Span::styled(
                option.label.clone(),
                theme.fg(if selected { "accent" } else { "text" }),
            ));
            out.extend(lines::text(&[Line::from(spans)], width, 1, 0, None));
        }
        out.extend(lines::spacer(1));
        let mut hint = ui.raw_key_hint("↑↓", "navigate");
        hint.push(Span::raw("  "));
        hint.extend(ui.key_hint("tui.select.confirm", "save"));
        hint.push(Span::raw("  "));
        hint.extend(ui.key_hint("tui.select.cancel", "cancel"));
        out.extend(lines::text(&[Line::from(hint)], width, 1, 0, None));
        out.extend(lines::spacer(1));
        out.push(ui.border(width));
        out
    }

    fn handle_input(&mut self, data: &str, ui: &Ui<'_>) -> Outcome {
        let kb = ui.keys;
        if kb.matches(data, "tui.select.up") || data == "k" {
            self.selected = self.selected.saturating_sub(1);
        } else if kb.matches(data, "tui.select.down") || data == "j" {
            self.selected = (self.selected + 1).min(self.options.len().saturating_sub(1));
        } else if kb.matches(data, "tui.select.confirm") || data == "\n" {
            if let Some(option) = self.options.get(self.selected) {
                return Outcome::Done(Action::Trust(Box::new(option.clone())));
            }
        } else if kb.matches(data, "tui.select.cancel") {
            return Outcome::Cancel;
        }
        Outcome::None
    }
}

/// pi's `ExtensionSelectorComponent`: a titled list of options.
pub struct ChoiceDialog {
    title: String,
    options: Vec<String>,
    selected: usize,
    /// When the dialog closes on its own.
    pub countdown: Option<Countdown>,
    /// Text under the title.
    pub description: Option<String>,
    /// Styled text after the option with this index.
    pub suffix: Option<(usize, Vec<Span<'static>>)>,
    /// The option with this index shimmers in the Radius colors while
    /// selected, as pi's `/login` menu, animated from this start.
    pub shimmer: Option<(usize, Instant)>,
}

/// The Radius logo colors, in the order they stream across the text.
const RADIUS_COLORS: [[f64; 3]; 4] = [
    [77.0, 154.0, 191.0],
    [131.0, 204.0, 210.0],
    [241.0, 190.0, 87.0],
    [240.0, 144.0, 130.0],
];
/// How often the shimmer moves.
pub const SHIMMER_FRAME: std::time::Duration = std::time::Duration::from_millis(50);

/// pi's `radiusShimmer`: `text` in the Radius colors flowing left to right,
/// 4 characters per color at 10 characters a second.
fn radius_shimmer(text: &str, elapsed: std::time::Duration, mode: ColorMode) -> Vec<Span<'static>> {
    const PER_COLOR: f64 = 4.0;
    let cycle = RADIUS_COLORS.len() as f64 * PER_COLOR;
    let offset = elapsed.as_secs_f64() * 10.0;
    text.chars()
        .enumerate()
        .map(|(index, ch)| {
            let position = ((index as f64 - offset) % cycle + cycle) % cycle;
            let band = (position / PER_COLOR).floor();
            let t = position / PER_COLOR - band;
            // Smoothstep keeps each band recognizable while blending.
            let amount = t * t * (3.0 - 2.0 * t);
            let from = RADIUS_COLORS[band as usize % RADIUS_COLORS.len()];
            let to = RADIUS_COLORS[(band as usize + 1) % RADIUS_COLORS.len()];
            let [r, g, b] = [0, 1, 2].map(|i| from[i] + (to[i] - from[i]) * amount);
            let color = ri_tui::color::Color::Rgb(r, g, b).to_terminal(mode);
            Span::styled(ch.to_string(), Style::default().fg(color))
        })
        .collect()
}

impl ChoiceDialog {
    /// A dialog titled `title` over `options`.
    pub fn new(title: &str, options: &[&str]) -> ChoiceDialog {
        ChoiceDialog {
            title: title.to_owned(),
            options: options.iter().map(|option| (*option).to_owned()).collect(),
            selected: 0,
            countdown: None,
            description: None,
            suffix: None,
            shimmer: None,
        }
    }

    /// Whether the selected option shimmers, which needs animation frames.
    pub fn shimmering(&self) -> bool {
        self.shimmer
            .is_some_and(|(index, _)| index == self.selected)
    }

    fn render(&self, width: usize, ui: &Ui<'_>) -> Vec<StyledLine> {
        let theme = ui.theme;
        let mut out = vec![ui.border(width)];
        out.extend(lines::spacer(1));
        let title = match self.countdown {
            Some(countdown) => countdown.title(&self.title),
            None => self.title.clone(),
        };
        let title: Vec<StyledLine> = title
            .split('\n')
            .map(|line| styled(line, theme.fg("accent").add_modifier(Modifier::BOLD)))
            .collect();
        out.extend(lines::text(&title, width, 1, 0, None));
        if let Some(description) = &self.description {
            out.extend(lines::spacer(1));
            let description: Vec<StyledLine> = description
                .split('\n')
                .map(|line| styled(line, theme.fg("text")))
                .collect();
            out.extend(lines::text(&description, width, 1, 0, None));
        }
        out.extend(lines::spacer(1));
        for (index, option) in self.options.iter().enumerate() {
            let mut spans = if index == self.selected {
                let mut spans = vec![Span::styled("→ ", theme.fg("accent"))];
                match self.shimmer {
                    Some((shimmer, start)) if shimmer == index => {
                        spans.extend(radius_shimmer(option, start.elapsed(), theme.mode()));
                    }
                    _ => spans.push(Span::styled(option.clone(), theme.fg("accent"))),
                }
                spans
            } else {
                vec![
                    Span::raw("  "),
                    Span::styled(option.clone(), theme.fg("text")),
                ]
            };
            if let Some((_, suffix)) = self.suffix.as_ref().filter(|(at, _)| *at == index) {
                spans.extend(suffix.iter().cloned());
            }
            out.extend(lines::text(&[Line::from(spans)], width, 1, 0, None));
        }
        out.extend(lines::spacer(1));
        let mut hint = ui.raw_key_hint("↑↓", "navigate");
        hint.push(Span::raw("  "));
        hint.extend(ui.key_hint("tui.select.confirm", "select"));
        hint.push(Span::raw("  "));
        hint.extend(ui.key_hint("tui.select.cancel", "cancel"));
        out.extend(lines::text(&[Line::from(hint)], width, 1, 0, None));
        out.extend(lines::spacer(1));
        out.push(ui.border(width));
        out
    }

    fn handle_input(&mut self, data: &str, ui: &Ui<'_>) -> Outcome {
        let kb = ui.keys;
        if kb.matches(data, "app.tools.expand") {
            return Outcome::Side(Action::ToggleTools);
        }
        if kb.matches(data, "tui.select.up") || data == "k" {
            self.selected = self.selected.saturating_sub(1);
        } else if kb.matches(data, "tui.select.down") || data == "j" {
            self.selected = (self.selected + 1).min(self.options.len().saturating_sub(1));
        } else if kb.matches(data, "tui.select.confirm") || data == "\n" {
            if self.selected < self.options.len() {
                return Outcome::Done(Action::Choice(self.selected));
            }
        } else if kb.matches(data, "tui.select.cancel") {
            return Outcome::Cancel;
        }
        Outcome::None
    }
}

/// pi's `ExtensionEditorComponent`: a titled multi-line editor.
pub struct TextDialog {
    title: String,
    editor: Editor,
}

impl TextDialog {
    /// A dialog titled `title` with `theme` for its editor.
    pub fn new(title: &str, theme: EditorTheme) -> TextDialog {
        let mut editor = Editor::new(theme, 0, 5);
        editor.focused = true;
        TextDialog {
            title: title.to_owned(),
            editor,
        }
    }

    /// Starts the editor with `text`.
    pub fn prefill(&mut self, text: &str) {
        self.editor.set_text(text);
    }

    fn render(&mut self, width: usize, ui: &Ui<'_>) -> (Vec<StyledLine>, Option<(usize, usize)>) {
        let theme = ui.theme;
        let mut out = vec![ui.border(width)];
        out.extend(lines::spacer(1));
        out.extend(lines::text(
            &[styled(self.title.clone(), theme.fg("accent"))],
            width,
            1,
            0,
            None,
        ));
        out.extend(lines::spacer(1));
        let editor = self.editor.render(width);
        let cursor = self
            .editor
            .cursor_position()
            .map(|(row, col)| (out.len() + row, col));
        out.extend(editor);
        out.extend(lines::spacer(1));
        let mut hint = ui.key_hint("tui.select.confirm", "submit");
        hint.push(Span::raw("  "));
        hint.extend(ui.key_hint("tui.input.newLine", "newline"));
        hint.push(Span::raw("  "));
        hint.extend(ui.key_hint("tui.select.cancel", "cancel"));
        hint.push(Span::raw("  "));
        hint.extend(ui.key_hint("app.editor.external", "external editor"));
        out.extend(lines::text(&[Line::from(hint)], width, 1, 0, None));
        out.extend(lines::spacer(1));
        out.push(ui.border(width));
        (out, cursor)
    }

    fn handle_input(&mut self, data: &str, ui: &Ui<'_>) -> Outcome {
        if ui.keys.matches(data, "tui.select.cancel") {
            return Outcome::Cancel;
        }
        match self.editor.handle_input(data, ui.keys) {
            EditorEvent::Submit(text) => Outcome::Done(Action::Text(text)),
            EditorEvent::None => Outcome::None,
        }
    }
}

/// pi's `ExtensionInputComponent`: a titled one-line input.
pub struct InputDialog {
    title: String,
    input: TextInput,
    /// When the dialog closes on its own.
    pub countdown: Option<Countdown>,
}

impl InputDialog {
    /// A dialog titled `title`.
    pub fn new(title: &str) -> InputDialog {
        let mut input = TextInput::default();
        input.focused = true;
        InputDialog {
            title: title.to_owned(),
            input,
            countdown: None,
        }
    }

    fn render(&mut self, width: usize, ui: &Ui<'_>) -> (Vec<StyledLine>, Option<(usize, usize)>) {
        let theme = ui.theme;
        let mut out = vec![ui.border(width)];
        out.extend(lines::spacer(1));
        let title = match self.countdown {
            Some(countdown) => countdown.title(&self.title),
            None => self.title.clone(),
        };
        out.extend(lines::text(
            &[styled(title, theme.fg("accent"))],
            width,
            1,
            0,
            None,
        ));
        out.extend(lines::spacer(1));
        let row = out.len();
        out.push(self.input.render(width));
        let cursor = self.input.cursor_column().map(|column| (row, column));
        out.extend(lines::spacer(1));
        let mut hint = ui.key_hint("tui.select.confirm", "submit");
        hint.push(Span::raw("  "));
        hint.extend(ui.key_hint("tui.select.cancel", "cancel"));
        out.extend(lines::text(&[Line::from(hint)], width, 1, 0, None));
        out.extend(lines::spacer(1));
        out.push(ui.border(width));
        (out, cursor)
    }

    fn handle_input(&mut self, data: &str, ui: &Ui<'_>) -> Outcome {
        let kb = ui.keys;
        if kb.matches(data, "tui.select.confirm") || data == "\n" {
            return Outcome::Done(Action::Text(self.input.value().to_owned()));
        }
        if kb.matches(data, "tui.select.cancel") {
            return Outcome::Cancel;
        }
        self.input.handle_input(data, kb);
        Outcome::None
    }
}
