//! A list of settings: each row a label and a value that Enter or Space
//! cycles, or a submenu its owner opens.
//!
//! Port of `packages/tui/src/components/settings-list.ts` in pi `v1.0.0`.
//! pi's submenus are components the list hosts; here the list reports which
//! item asked for one and its owner draws it.

use ratatui_core::style::Style;
use ratatui_core::text::{Line, Span};

use crate::fuzzy::fuzzy_filter;
use crate::keybindings::Keybindings;
use crate::lines::{self, StyledLine, styled};
use crate::screen::MouseKind;
use crate::select_list::{nudge, step, visible_range};
use crate::text::{truncate_to_width, visible_width};
use crate::text_input::TextInput;

const MAX_LABEL_WIDTH: usize = 36;

/// One setting.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SettingItem {
    /// Identifies the setting in events.
    pub id: String,
    /// The left column.
    pub label: String,
    /// Shown below the list while the item is selected.
    pub description: Option<String>,
    /// The right column.
    pub current_value: String,
    /// Values Enter and Space cycle through.
    pub values: Vec<String>,
    /// Enter and Space open a submenu instead.
    pub submenu: bool,
}

/// Styles for a settings list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SettingsListTheme {
    /// The selected row's label, value and cursor.
    pub selected: Style,
    /// Values of other rows.
    pub value: Style,
    /// The selected item's description.
    pub description: Style,
    /// Hints and the scroll position.
    pub hint: Style,
}

/// What a key did to the list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SettingsEvent {
    /// Nothing for the owner to do.
    None,
    /// A value was cycled to `value`.
    Changed {
        /// The setting.
        id: String,
        /// Its new value.
        value: String,
    },
    /// The item with this id asks for its submenu.
    Open(String),
    /// Escape.
    Cancelled,
}

/// A settings list.
#[derive(Clone, Debug)]
pub struct SettingsList {
    items: Vec<SettingItem>,
    filtered: Vec<usize>,
    selected: usize,
    max_visible: usize,
    theme: SettingsListTheme,
    search: Option<TextInput>,
    /// The item a press highlighted, which the click activates.
    pressed: Option<usize>,
}

impl SettingsList {
    /// A list showing at most `max_visible` items at a time, with a search
    /// field above them when `search` is set.
    pub fn new(
        items: Vec<SettingItem>,
        max_visible: usize,
        theme: SettingsListTheme,
        search: bool,
    ) -> SettingsList {
        let filtered = (0..items.len()).collect();
        SettingsList {
            items,
            filtered,
            selected: 0,
            max_visible,
            theme,
            search: search.then(TextInput::default),
            pressed: None,
        }
    }

    /// Sets the value shown for `id`.
    pub fn update_value(&mut self, id: &str, value: &str) {
        if let Some(item) = self.items.iter_mut().find(|item| item.id == id) {
            value.clone_into(&mut item.current_value);
        }
    }

    /// The value shown for `id`.
    pub fn value(&self, id: &str) -> Option<&str> {
        self.items
            .iter()
            .find(|item| item.id == id)
            .map(|item| item.current_value.as_str())
    }

    fn shown(&self) -> Vec<&SettingItem> {
        self.filtered
            .iter()
            .map(|&index| &self.items[index])
            .collect()
    }

    /// The list's rows at `width`.
    pub fn render(&mut self, width: usize) -> Vec<StyledLine> {
        let theme = self.theme;
        let mut out = Vec::new();
        if let Some(input) = &mut self.search {
            out.push(input.render(width));
            out.push(Line::default());
        }
        if self.items.is_empty() {
            out.push(styled("  No settings available", theme.hint));
            if self.search.is_some() {
                self.hint(&mut out, width);
            }
            return out;
        }
        let shown = self.shown();
        if shown.is_empty() {
            out.push(lines::truncate(
                &styled("  No matching settings", theme.hint),
                width,
                "...",
            ));
            self.hint(&mut out, width);
            return out;
        }
        let (start, end) = visible_range(self.selected, shown.len(), self.max_visible);
        let label_width = self
            .items
            .iter()
            .map(|item| visible_width(&item.label))
            .max()
            .unwrap_or(0)
            .min(MAX_LABEL_WIDTH);
        for (index, item) in shown.iter().enumerate().take(end).skip(start) {
            let selected = index == self.selected;
            let (prefix, label_style, value_style) = if selected {
                (
                    Span::styled("→ ", theme.selected),
                    theme.selected,
                    theme.selected,
                )
            } else {
                (Span::raw("  "), Style::default(), theme.value)
            };
            let padding = label_width.saturating_sub(visible_width(&item.label));
            let label = format!("{}{}", item.label, " ".repeat(padding));
            let value_width = width.saturating_sub(2 + label_width + 2 + 2);
            let value = truncate_to_width(&item.current_value, value_width, "", false);
            let line = Line::from(vec![
                prefix,
                Span::styled(label, label_style),
                Span::raw("  "),
                Span::styled(value, value_style),
            ]);
            out.push(lines::truncate(&line, width, "..."));
        }
        if start > 0 || end < shown.len() {
            let text = format!("  ({}/{})", self.selected + 1, shown.len());
            out.push(styled(
                truncate_to_width(&text, width.saturating_sub(2), "", false),
                theme.hint,
            ));
        }
        if let Some(description) = shown
            .get(self.selected)
            .and_then(|item| item.description.as_deref())
        {
            out.push(Line::default());
            for line in lines::wrap(&lines::raw(description), width.saturating_sub(4)) {
                out.push(styled(
                    format!("  {}", lines::plain(&line)),
                    theme.description,
                ));
            }
        }
        self.hint(&mut out, width);
        out
    }

    fn hint(&self, out: &mut Vec<StyledLine>, width: usize) {
        let text = if self.search.is_some() {
            "  Type to search · Enter/Space to change · Esc to cancel"
        } else {
            "  Enter/Space to change · Esc to cancel"
        };
        out.push(Line::default());
        out.push(lines::truncate(
            &styled(text, self.theme.hint),
            width,
            "...",
        ));
    }

    /// Handles a key.
    pub fn handle_input(&mut self, data: &str, keybindings: &Keybindings) -> SettingsEvent {
        let count = self.filtered.len();
        let search_empty = self
            .search
            .as_ref()
            .is_none_or(|input| input.value().is_empty());
        if keybindings.matches(data, "tui.select.up") {
            self.selected = step(self.selected, count, false);
        } else if keybindings.matches(data, "tui.select.down") {
            self.selected = step(self.selected, count, true);
        } else if keybindings.matches(data, "tui.select.confirm") || (data == " " && search_empty) {
            return self.activate();
        } else if keybindings.matches(data, "tui.select.cancel") {
            return SettingsEvent::Cancelled;
        } else if let Some(input) = &mut self.search {
            input.handle_input(data, keybindings);
            let query = input.value().to_owned();
            let indices: Vec<usize> = (0..self.items.len()).collect();
            self.filtered = fuzzy_filter(indices, &query, |index| self.items[*index].label.clone());
            self.selected = 0;
        }
        SettingsEvent::None
    }

    /// pi's `handleMouse`: a press on the search field moves its cursor, the
    /// wheel moves the highlight a row without wrapping, a press highlights
    /// the item under it and a click activates the item pressed, as Enter
    /// does. `x` and `y` are a column and row of [`SettingsList::render`]'s;
    /// `None` when the list does not take the event.
    pub fn mouse(&mut self, kind: MouseKind, x: usize, y: usize) -> Option<SettingsEvent> {
        let mut y = y;
        if let Some(input) = &mut self.search {
            match y {
                0 => return input.mouse(kind, x).then_some(SettingsEvent::None),
                1 => return None,
                _ => y -= 2,
            }
        }
        let count = self.filtered.len();
        if count == 0 {
            return None;
        }
        let (start, end) = visible_range(self.selected, count, self.max_visible);
        let index = start + y;
        match kind {
            MouseKind::Wheel(direction) => {
                self.selected = nudge(self.selected, count, direction > 0);
            }
            _ if index >= end => return None,
            MouseKind::Press => {
                self.pressed = Some(index);
                self.selected = index;
            }
            MouseKind::Click => {
                self.selected = self.pressed.take().unwrap_or(index);
                return Some(self.activate());
            }
        }
        Some(SettingsEvent::None)
    }

    fn activate(&mut self) -> SettingsEvent {
        let Some(&index) = self.filtered.get(self.selected) else {
            return SettingsEvent::None;
        };
        let item = &mut self.items[index];
        if item.submenu {
            return SettingsEvent::Open(item.id.clone());
        }
        if item.values.is_empty() {
            return SettingsEvent::None;
        }
        let next = item
            .values
            .iter()
            .position(|value| *value == item.current_value)
            .map_or(0, |position| (position + 1) % item.values.len());
        item.current_value = item.values[next].clone();
        SettingsEvent::Changed {
            id: item.id.clone(),
            value: item.current_value.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, value: &str, values: &[&str]) -> SettingItem {
        SettingItem {
            id: id.to_owned(),
            label: id.to_owned(),
            description: None,
            current_value: value.to_owned(),
            values: values.iter().map(|value| (*value).to_owned()).collect(),
            submenu: false,
        }
    }

    fn text(list: &mut SettingsList, width: usize) -> Vec<String> {
        list.render(width)
            .iter()
            .map(|line| lines::plain(line).trim_end().to_owned())
            .collect()
    }

    #[test]
    fn cycles_values_and_scrolls_as_pi_tui() {
        let items = (0..5)
            .map(|n| item(&format!("item{n}"), "true", &["true", "false"]))
            .collect();
        let mut list = SettingsList::new(items, 3, SettingsListTheme::default(), false);
        let keys = Keybindings::new(
            crate::keys::Keys::default(),
            crate::keybindings::tui_definitions(),
            &crate::keybindings::UserBindings::new(),
        );
        assert_eq!(
            text(&mut list, 40),
            [
                "→ item0  true",
                "  item1  true",
                "  item2  true",
                "  (1/5)",
                "",
                "  Enter/Space to change · Esc to cancel"
            ]
        );
        assert_eq!(
            list.handle_input(" ", &keys),
            SettingsEvent::Changed {
                id: "item0".into(),
                value: "false".into()
            }
        );
        list.handle_input("\x1b[A", &keys);
        assert_eq!(
            &text(&mut list, 40)[..4],
            ["  item2  true", "  item3  true", "→ item4  true", "  (5/5)"]
        );
        assert_eq!(list.handle_input("\x1b", &keys), SettingsEvent::Cancelled);
    }
}
