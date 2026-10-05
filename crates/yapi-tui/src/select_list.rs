//! A scrolling list of items with an optional description column.
//!
//! Port of `packages/tui/src/components/select-list.ts` in pi `v1.0.0`.

use ratatui_core::style::Style;
use ratatui_core::text::{Line, Span};

use crate::keybindings::Keybindings;
use crate::text::{truncate_to_width, visible_width};

const DEFAULT_PRIMARY_COLUMN_WIDTH: usize = 32;
const PRIMARY_COLUMN_GAP: usize = 2;
const MIN_DESCRIPTION_WIDTH: usize = 10;

/// One entry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SelectItem {
    /// The value returned on selection, and the filter key.
    pub value: String,
    /// Display text; the value when empty.
    pub label: String,
    /// Shown in a second column when there is room.
    pub description: Option<String>,
}

impl SelectItem {
    /// An item whose label is its value.
    pub fn new(value: impl Into<String>) -> SelectItem {
        let value = value.into();
        SelectItem {
            label: value.clone(),
            value,
            description: None,
        }
    }
}

/// Styles for a select list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SelectListTheme {
    /// The highlighted row.
    pub selected_text: Style,
    /// Description column of other rows.
    pub description: Style,
    /// The `(n/total)` line.
    pub scroll_info: Style,
    /// The empty-list message.
    pub no_match: Style,
}

/// Bounds for the first column.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SelectListLayout {
    /// Minimum width of the value column, including its gap.
    pub min_primary_column_width: Option<usize>,
    /// Maximum width of the value column, including its gap.
    pub max_primary_column_width: Option<usize>,
}

/// What a key did to the list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SelectEvent {
    /// The key was not a list key.
    Ignored,
    /// The highlight moved.
    Moved,
    /// The highlighted item was chosen.
    Selected(SelectItem),
    /// The list was dismissed.
    Cancelled,
}

/// A select list.
#[derive(Clone, Debug)]
pub struct SelectList {
    items: Vec<SelectItem>,
    selected: usize,
    max_visible: usize,
    theme: SelectListTheme,
    layout: SelectListLayout,
}

/// The first and one-past-last of `count` rows a window of `max_visible`
/// shows around `selected`, as pi-tui's lists scroll.
pub fn visible_range(selected: usize, count: usize, max_visible: usize) -> (usize, usize) {
    let start = selected
        .saturating_sub(max_visible / 2)
        .min(count.saturating_sub(max_visible));
    (start, (start + max_visible).min(count))
}

/// The row after `selected`, or before it unless `forward`, wrapping around
/// `count` rows.
pub fn step(selected: usize, count: usize, forward: bool) -> usize {
    match count {
        0 => 0,
        _ if forward => (selected + 1) % count,
        _ => (selected + count - 1) % count,
    }
}

fn single_line(text: &str) -> String {
    text.split(['\r', '\n'])
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_owned()
}

impl SelectList {
    /// A list showing at most `max_visible` rows at a time.
    pub fn new(
        items: Vec<SelectItem>,
        max_visible: usize,
        theme: SelectListTheme,
        layout: SelectListLayout,
    ) -> SelectList {
        SelectList {
            items,
            selected: 0,
            max_visible,
            theme,
            layout,
        }
    }

    /// Highlights the `index`th visible item, clamped to the list.
    pub fn set_selected_index(&mut self, index: usize) {
        self.selected = index.min(self.items.len().saturating_sub(1));
    }

    /// The highlighted item.
    pub fn selected_item(&self) -> Option<&SelectItem> {
        self.items.get(self.selected)
    }

    /// Handles a key.
    pub fn handle_input(&mut self, data: &str, keybindings: &Keybindings) -> SelectEvent {
        if keybindings.matches(data, "tui.select.up") {
            self.selected = step(self.selected, self.items.len(), false);
            SelectEvent::Moved
        } else if keybindings.matches(data, "tui.select.down") {
            self.selected = step(self.selected, self.items.len(), true);
            SelectEvent::Moved
        } else if keybindings.matches(data, "tui.select.confirm") {
            match self.selected_item() {
                Some(item) => SelectEvent::Selected(item.clone()),
                None => SelectEvent::Ignored,
            }
        } else if keybindings.matches(data, "tui.select.cancel") {
            SelectEvent::Cancelled
        } else {
            SelectEvent::Ignored
        }
    }

    fn display_value(item: &SelectItem) -> &str {
        if item.label.is_empty() {
            &item.value
        } else {
            &item.label
        }
    }

    fn primary_column_width(&self) -> usize {
        let raw_min = self
            .layout
            .min_primary_column_width
            .or(self.layout.max_primary_column_width)
            .unwrap_or(DEFAULT_PRIMARY_COLUMN_WIDTH);
        let raw_max = self
            .layout
            .max_primary_column_width
            .or(self.layout.min_primary_column_width)
            .unwrap_or(DEFAULT_PRIMARY_COLUMN_WIDTH);
        let (min, max) = (raw_min.min(raw_max).max(1), raw_min.max(raw_max).max(1));
        let widest = self
            .items
            .iter()
            .map(|item| visible_width(Self::display_value(item)) + PRIMARY_COLUMN_GAP)
            .max()
            .unwrap_or(0);
        widest.clamp(min, max)
    }

    /// The rows for `width` columns.
    pub fn render(&self, width: usize) -> Vec<Line<'static>> {
        if self.items.is_empty() {
            return vec![Line::from(Span::styled(
                "  No matching commands",
                self.theme.no_match,
            ))];
        }
        let primary = self.primary_column_width();
        let (start, end) = visible_range(self.selected, self.items.len(), self.max_visible);
        let mut lines: Vec<Line<'static>> = (start..end)
            .map(|position| {
                self.render_item(
                    &self.items[position],
                    position == self.selected,
                    width,
                    primary,
                )
            })
            .collect();
        if start > 0 || end < self.items.len() {
            let text = format!("  ({}/{})", self.selected + 1, self.items.len());
            lines.push(Line::from(Span::styled(
                truncate_to_width(&text, width.saturating_sub(2), "", false),
                self.theme.scroll_info,
            )));
        }
        lines
    }

    fn render_item(
        &self,
        item: &SelectItem,
        selected: bool,
        width: usize,
        primary: usize,
    ) -> Line<'static> {
        let prefix = if selected { "→ " } else { "  " };
        let prefix_width = 2;
        let value = Self::display_value(item);
        let description = item.description.as_deref().map(single_line);
        if let Some(description) = description.filter(|d| !d.is_empty() && width > 40) {
            let column = primary.min(width.saturating_sub(prefix_width + 4)).max(1);
            let max_primary = column.saturating_sub(PRIMARY_COLUMN_GAP).max(1);
            let truncated = truncate_to_width(value, max_primary, "", false);
            let truncated_width = visible_width(&truncated);
            let spacing = " ".repeat(column.saturating_sub(truncated_width).max(1));
            let start = prefix_width + truncated_width + spacing.len();
            let remaining = width as isize - start as isize - 2;
            if remaining > MIN_DESCRIPTION_WIDTH as isize {
                let description = truncate_to_width(&description, remaining as usize, "", false);
                if selected {
                    return Line::from(Span::styled(
                        format!("{prefix}{truncated}{spacing}{description}"),
                        self.theme.selected_text,
                    ));
                }
                return Line::from(vec![
                    Span::raw(format!("{prefix}{truncated}")),
                    Span::styled(format!("{spacing}{description}"), self.theme.description),
                ]);
            }
        }
        let max_width = width.saturating_sub(prefix_width + 2);
        let truncated = truncate_to_width(value, max_width, "", false);
        if selected {
            Line::from(Span::styled(
                format!("{prefix}{truncated}"),
                self.theme.selected_text,
            ))
        } else {
            Line::from(format!("{prefix}{truncated}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keybindings::{UserBindings, tui_definitions};
    use crate::keys::Keys;

    fn plain(lines: &[Line<'_>]) -> Vec<String> {
        lines.iter().map(ToString::to_string).collect()
    }

    fn items(count: usize) -> Vec<SelectItem> {
        (0..count)
            .map(|index| SelectItem {
                value: format!("item{index}"),
                label: String::new(),
                description: Some(format!("Description\nof {index}")),
            })
            .collect()
    }

    #[test]
    fn renders_columns_and_scroll_info() {
        let mut list = SelectList::new(
            items(8),
            3,
            SelectListTheme::default(),
            SelectListLayout::default(),
        );
        list.set_selected_index(4);
        assert_eq!(
            plain(&list.render(60)),
            [
                "  item3                           Description of 3",
                "→ item4                           Description of 4",
                "  item5                           Description of 5",
                "  (5/8)",
            ]
        );
        assert_eq!(
            plain(&list.render(30)),
            ["  item3", "→ item4", "  item5", "  (5/8)"]
        );
    }

    #[test]
    fn wraps_selection_and_filters() {
        let keybindings = Keybindings::new(
            Keys::default(),
            tui_definitions(),
            &UserBindings::new(),
            &[],
        );
        let mut list = SelectList::new(
            items(3),
            5,
            SelectListTheme::default(),
            SelectListLayout::default(),
        );
        assert_eq!(
            list.handle_input("\x1b[A", &keybindings),
            SelectEvent::Moved
        );
        assert_eq!(
            list.selected_item().map(|item| item.value.as_str()),
            Some("item2")
        );
        assert_eq!(
            list.handle_input("\r", &keybindings),
            SelectEvent::Selected(items(3)[2].clone())
        );
        let empty = SelectList::new(
            Vec::new(),
            5,
            SelectListTheme::default(),
            SelectListLayout::default(),
        );
        assert_eq!(plain(&empty.render(40)), ["  No matching commands"]);
    }
}
