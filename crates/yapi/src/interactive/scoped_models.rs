//! The `/scoped-models` selector: which models `ctrl+p` cycles through, in
//! which order, for the session or saved to settings.
//!
//! Port of `scoped-models-selector.ts` in
//! `packages/coding-agent/src/modes/interactive/components` in pi `v1.0.0`.

use ratatui_core::style::Modifier;
use ratatui_core::text::{Line, Span};
use yapi_tui::fuzzy::fuzzy_filter;
use yapi_tui::lines::{self, StyledLine, styled};
use yapi_tui::select_list::{step, visible_range};
use yapi_tui::text_input::TextInput;
use yapi_types::model::Model;

use super::catalogs::RefreshStatus;
use super::keybindings::keys_display;
use super::selectors::{Action, Outcome, Ui};

/// Enabled model ids in order; `None` enables every model.
type Enabled = Option<Vec<String>>;

fn is_enabled(enabled: &Enabled, id: &str) -> bool {
    enabled
        .as_ref()
        .is_none_or(|ids| ids.iter().any(|item| item == id))
}

/// An explicit list that covers every model collapses to `None`.
fn normalize(result: Vec<String>, all: &[String]) -> Enabled {
    let covers = result.len() == all.len() && result.iter().all(|id| all.contains(id));
    (!covers).then_some(result)
}

fn toggle(enabled: &Enabled, all: &[String], id: &str) -> Enabled {
    match enabled {
        None => Some(all.iter().filter(|item| *item != id).cloned().collect()),
        Some(ids) => match ids.iter().position(|item| item == id) {
            Some(index) => {
                let mut ids = ids.clone();
                ids.remove(index);
                Some(ids)
            }
            None => {
                let mut ids = ids.clone();
                ids.push(id.to_owned());
                normalize(ids, all)
            }
        },
    }
}

fn enable_all(enabled: &Enabled, all: &[String], targets: Option<&[String]>) -> Enabled {
    let ids = enabled.as_ref()?;
    let mut result = ids.clone();
    for id in targets.unwrap_or(all) {
        if !result.contains(id) {
            result.push(id.clone());
        }
    }
    normalize(result, all)
}

fn clear_all(enabled: &Enabled, all: &[String], targets: Option<&[String]>) -> Enabled {
    match enabled {
        None => Some(match targets {
            Some(targets) => all
                .iter()
                .filter(|id| !targets.contains(id))
                .cloned()
                .collect(),
            None => Vec::new(),
        }),
        Some(ids) => {
            let targets = targets.unwrap_or(ids);
            Some(
                ids.iter()
                    .filter(|id| !targets.contains(id))
                    .cloned()
                    .collect(),
            )
        }
    }
}

fn sorted_ids(enabled: &Enabled, all: &[String]) -> Vec<String> {
    match enabled {
        None => all.to_vec(),
        Some(ids) => ids
            .iter()
            .cloned()
            .chain(all.iter().filter(|id| !ids.contains(id)).cloned())
            .collect(),
    }
}

/// pi's `getModelSearchText`.
pub(super) fn search_text(model: &Model) -> String {
    let name = if model.name.is_empty() {
        String::new()
    } else {
        format!(" {}", model.name)
    };
    format!(
        "{id} {provider} {provider}/{id} {provider} {id}{name}",
        id = model.id,
        provider = model.provider
    )
}

#[derive(Clone)]
struct Row {
    id: String,
    model: Option<Model>,
    enabled: bool,
}

/// The `/scoped-models` selector.
pub struct ScopedModelsSelector {
    models: Vec<Model>,
    all: Vec<String>,
    enabled: Enabled,
    rows: Vec<Row>,
    selected: usize,
    input: TextInput,
    dirty: bool,
    /// The selection changed since the selector opened.
    touched: bool,
    /// The catalog refresh this selector waits on.
    pub refresh_id: u64,
    catalogs: RefreshStatus,
}

const MAX_VISIBLE: usize = 8;

impl ScopedModelsSelector {
    /// A selector over `models` with `enabled` checked.
    pub fn new(models: Vec<Model>, enabled: Enabled) -> ScopedModelsSelector {
        let all = models.iter().map(Model::reference).collect();
        let mut input = TextInput::default();
        input.focused = true;
        let mut selector = ScopedModelsSelector {
            models,
            all,
            enabled,
            rows: Vec::new(),
            selected: 0,
            input,
            dirty: false,
            touched: false,
            refresh_id: 0,
            catalogs: RefreshStatus::Running,
        };
        selector.refresh();
        selector
    }

    /// Whether the selection changed since the selector opened.
    pub fn touched(&self) -> bool {
        self.touched
    }

    /// The enabled model ids; `None` enables every model.
    pub fn enabled(&self) -> &Enabled {
        &self.enabled
    }

    /// The catalog refresh finished: pi's `updateModels` with the refreshed
    /// `models`, and `enabled` when the selection follows the settings.
    pub fn refreshed(
        &mut self,
        models: Vec<Model>,
        enabled: Option<Enabled>,
        status: RefreshStatus,
    ) {
        let selected = self.rows.get(self.selected).map(|row| row.id.clone());
        if let Some(enabled) = enabled {
            self.enabled = enabled;
        }
        self.all = models.iter().map(Model::reference).collect();
        self.models = models;
        self.catalogs = status;
        self.refresh();
        if let Some(index) = selected.and_then(|id| self.rows.iter().position(|row| row.id == id)) {
            self.selected = index;
        }
    }

    fn model(&self, id: &str) -> Option<&Model> {
        self.models.iter().find(|model| model.reference() == id)
    }

    fn refresh(&mut self) {
        let rows: Vec<Row> = sorted_ids(&self.enabled, &self.all)
            .into_iter()
            .map(|id| Row {
                model: self.model(&id).cloned(),
                enabled: is_enabled(&self.enabled, &id),
                id,
            })
            .collect();
        let query = self.input.value().to_owned();
        self.rows = if query.is_empty() {
            rows
        } else {
            fuzzy_filter(rows, &query, |row| match &row.model {
                Some(model) => search_text(model),
                None => row.id.clone(),
            })
        };
        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
    }

    fn footer(&self, ui: &Ui<'_>) -> StyledLine {
        let count = match &self.enabled {
            None => "all enabled".to_owned(),
            Some(ids) => {
                let available = ids.iter().filter(|id| self.model(id).is_some()).count();
                let missing = ids.len() - available;
                let missing = if missing > 0 {
                    format!(" · {missing} unavailable")
                } else {
                    String::new()
                };
                format!("{available}/{} enabled{missing}", self.all.len())
            }
        };
        let key = |action: &str| keys_display(ui.keys, action);
        let parts = [
            format!("{} toggle", key("tui.select.confirm")),
            format!("{} all", key("app.models.enableAll")),
            format!("{} clear", key("app.models.clearAll")),
            format!("{} provider", key("app.models.toggleProvider")),
            format!(
                "{}/{} reorder",
                key("app.models.reorderUp"),
                key("app.models.reorderDown")
            ),
            format!("{} save", key("app.models.save")),
            count,
        ];
        let dim = ui.theme.fg("dim");
        if self.dirty {
            Line::from(vec![
                Span::styled(format!("  {} ", parts.join(" · ")), dim),
                Span::styled("(unsaved)", ui.theme.fg("warning")),
            ])
        } else {
            styled(format!("  {}", parts.join(" · ")), dim)
        }
    }

    /// The rows at `width`, and the terminal cursor among them.
    pub fn render(
        &mut self,
        width: usize,
        ui: &Ui<'_>,
    ) -> (Vec<StyledLine>, Option<(usize, usize)>) {
        let theme = ui.theme;
        let muted = theme.fg("muted");
        let accent = theme.fg("accent");
        let mut out = vec![ui.border(width)];
        out.extend(lines::spacer(1));
        out.extend(lines::text_row(
            styled("Model Configuration", accent.add_modifier(Modifier::BOLD)),
            width,
            0,
        ));
        out.extend(lines::text_row(
            styled(
                format!(
                    "Session-only. {} to save to settings.",
                    keys_display(ui.keys, "app.models.save")
                ),
                muted,
            ),
            width,
            0,
        ));
        out.extend(lines::spacer(1));
        let input = self.input.render(width);
        let cursor = self.input.cursor_column().map(|col| (out.len(), col));
        out.push(input);
        out.extend(lines::spacer(1));
        if self.rows.is_empty() {
            out.extend(lines::text_row(
                styled("  No matching models", muted),
                width,
                0,
            ));
        } else {
            let count = self.rows.len();
            let (start, end) = visible_range(self.selected, count, MAX_VISIBLE);
            for (index, item) in self.rows.iter().enumerate().take(end).skip(start) {
                let selected = index == self.selected;
                let id = item
                    .model
                    .as_ref()
                    .map_or(item.id.clone(), |model| model.id.clone());
                let mut id_style = if selected { accent } else { Default::default() };
                if item.model.is_none() {
                    id_style = id_style.add_modifier(Modifier::CROSSED_OUT);
                }
                let badge = match &item.model {
                    Some(model) => format!(" [{}]", model.provider),
                    None => " [unavailable]".to_owned(),
                };
                out.extend(lines::text_row(
                    Line::from(vec![
                        if selected {
                            Span::styled("→ ", accent)
                        } else {
                            Span::raw("  ")
                        },
                        if item.model.is_some() && item.enabled {
                            Span::styled("✓ ", accent)
                        } else {
                            Span::raw("  ")
                        },
                        Span::styled(id, id_style),
                        Span::styled(badge, muted),
                    ]),
                    width,
                    0,
                ));
            }
            if start > 0 || end < count {
                out.extend(lines::text_row(
                    styled(format!("  ({}/{count})", self.selected + 1), muted),
                    width,
                    0,
                ));
            }
            let name = match &self.rows[self.selected].model {
                Some(model) => format!("  Model Name: {}", model.name),
                None => "  Model unavailable".to_owned(),
            };
            out.extend(lines::spacer(1));
            out.extend(lines::text_row(styled(name, muted), width, 0));
        }
        out.extend(lines::spacer(1));
        let (text, color) = match &self.catalogs {
            RefreshStatus::Running => (RefreshStatus::RUNNING, "muted"),
            RefreshStatus::Done => (RefreshStatus::DONE, "success"),
            RefreshStatus::Failed(message) => (message.as_str(), "warning"),
        };
        out.extend(lines::text_row(
            styled(format!("  {text}"), theme.fg(color)),
            width,
            0,
        ));
        out.extend(lines::text_row(self.footer(ui), width, 0));
        out.push(ui.border(width));
        (out, cursor)
    }

    fn changed(&mut self) -> Outcome {
        self.dirty = true;
        self.touched = true;
        self.refresh();
        Outcome::Side(Action::ScopedModels {
            enabled: self.enabled.clone(),
            save: false,
        })
    }

    /// Handles a key.
    pub fn handle_input(&mut self, data: &str, ui: &Ui<'_>) -> Outcome {
        let kb = ui.keys;
        let count = self.rows.len();
        if kb.matches(data, "tui.select.up") {
            self.selected = step(self.selected, count, false);
            return Outcome::None;
        }
        if kb.matches(data, "tui.select.down") {
            self.selected = step(self.selected, count, true);
            return Outcome::None;
        }
        let up = kb.matches(data, "app.models.reorderUp");
        if up || kb.matches(data, "app.models.reorderDown") {
            let Some(ids) = &mut self.enabled else {
                return Outcome::None;
            };
            let Some(row) = self.rows.get(self.selected) else {
                return Outcome::None;
            };
            let Some(index) = ids.iter().position(|id| *id == row.id) else {
                return Outcome::None;
            };
            let target = if up {
                index.checked_sub(1)
            } else {
                Some(index + 1)
            };
            let Some(target) = target.filter(|target| *target < ids.len()) else {
                return Outcome::None;
            };
            ids.swap(index, target);
            self.selected = if up {
                self.selected.saturating_sub(1)
            } else {
                self.selected + 1
            };
            return self.changed();
        }
        if kb.matches(data, "tui.select.confirm") {
            let Some(row) = self.rows.get(self.selected) else {
                return Outcome::None;
            };
            self.enabled = toggle(&self.enabled, &self.all, &row.id.clone());
            return self.changed();
        }
        let searching = !self.input.value().is_empty();
        let shown: Vec<String> = self.rows.iter().map(|row| row.id.clone()).collect();
        let targets = searching.then_some(shown.as_slice());
        if kb.matches(data, "app.models.enableAll") {
            self.enabled = enable_all(&self.enabled, &self.all, targets);
            return self.changed();
        }
        if kb.matches(data, "app.models.clearAll") {
            self.enabled = clear_all(&self.enabled, &self.all, targets);
            return self.changed();
        }
        if kb.matches(data, "app.models.toggleProvider") {
            let Some(provider) = self
                .rows
                .get(self.selected)
                .and_then(|row| row.model.as_ref())
                .map(|model| model.provider.clone())
            else {
                return Outcome::None;
            };
            let ids: Vec<String> = self
                .models
                .iter()
                .filter(|model| model.provider == provider)
                .map(Model::reference)
                .collect();
            let all_on = ids.iter().all(|id| is_enabled(&self.enabled, id));
            self.enabled = if all_on {
                clear_all(&self.enabled, &self.all, Some(&ids))
            } else {
                enable_all(&self.enabled, &self.all, Some(&ids))
            };
            return self.changed();
        }
        if kb.matches(data, "app.models.save") {
            self.dirty = false;
            return Outcome::Side(Action::ScopedModels {
                enabled: self.enabled.clone(),
                save: true,
            });
        }
        let decoder = kb.decoder();
        if decoder.matches(data, "ctrl+c") {
            if self.input.value().is_empty() {
                return Outcome::Cancel;
            }
            self.input.set_value("");
            self.refresh();
            return Outcome::None;
        }
        if decoder.matches(data, "escape") {
            return Outcome::Cancel;
        }
        self.input.handle_input(data, kb);
        self.refresh();
        Outcome::None
    }
}

/// The ids the `enabledModels` patterns select, then the patterns that
/// select nothing; `None` without patterns.
pub(super) fn configured_ids(patterns: &[String], models: &[Model]) -> Enabled {
    if patterns.is_empty() {
        return None;
    }
    let (scoped, _) = yapi_core::model_resolver::resolve_model_scope(patterns, models);
    let mut ids: Vec<String> = scoped.iter().map(|entry| entry.model.reference()).collect();
    for pattern in patterns {
        let (found, _) =
            yapi_core::model_resolver::resolve_model_scope(std::slice::from_ref(pattern), models);
        if found.is_empty() && !ids.contains(pattern) {
            ids.push(pattern.clone());
        }
    }
    Some(ids)
}

impl super::App {
    /// pi's `showModelsSelector`.
    pub(super) fn open_scoped_models(&mut self) {
        let models = self.session.available_models();
        let scoped = self.session.scoped_models();
        let enabled = if scoped.is_empty() {
            configured_ids(
                &self.session.settings().enabled_models.unwrap_or_default(),
                &models,
            )
        } else {
            Some(scoped.iter().map(|entry| entry.model.reference()).collect())
        };
        let mut selector = ScopedModelsSelector::new(models, enabled);
        selector.refresh_id = self.next_refresh_id();
        let refresh = super::catalogs::Refresh::Selector(selector.refresh_id);
        self.selector = Some(super::Selector::ScopedModels(Box::new(selector)));
        self.refresh_catalogs(None, refresh);
    }

    /// Scopes the session to `enabled`, or with `save` writes it to the
    /// `enabledModels` setting, as pi's callbacks do.
    pub(super) fn scoped_models_changed(&mut self, enabled: Enabled, save: bool) {
        let models = self.session.available_models();
        let available: Vec<String> = models.iter().map(Model::reference).collect();
        let all_enabled = |ids: &[String]| available.iter().all(|id| ids.contains(id));
        if save {
            let value = enabled
                .and_then(|ids| normalize(ids, &available))
                .map(serde_json::Value::from);
            let _ = self.session.set_global_setting("enabledModels", value);
            self.status("Model selection saved to settings");
            return;
        }
        let scoped = match &enabled {
            Some(ids) if ids.iter().any(|id| available.contains(id)) && !all_enabled(ids) => {
                yapi_core::model_resolver::resolve_model_scope(ids, &models).0
            }
            _ => Vec::new(),
        };
        self.session.set_scoped_models(scoped);
        self.footer_cache = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn edits_the_enabled_list_as_pi() {
        let all = ids(&["a/1", "a/2", "b/1"]);
        let one_off = toggle(&None, &all, "a/2");
        assert_eq!(one_off, Some(ids(&["a/1", "b/1"])));
        assert_eq!(toggle(&one_off, &all, "a/2"), None);
        assert_eq!(clear_all(&None, &all, None), Some(Vec::new()));
        assert_eq!(
            clear_all(&None, &all, Some(&ids(&["a/1"]))),
            Some(ids(&["a/2", "b/1"]))
        );
        assert_eq!(
            enable_all(&Some(ids(&["b/1"])), &all, Some(&ids(&["a/1"]))),
            Some(ids(&["b/1", "a/1"]))
        );
        assert_eq!(enable_all(&Some(ids(&["b/1"])), &all, None), None);
        assert_eq!(
            sorted_ids(&Some(ids(&["b/1"])), &all),
            ids(&["b/1", "a/1", "a/2"])
        );
    }
}
