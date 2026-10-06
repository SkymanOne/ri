//! pi-tui's autocomplete shapes, as extensions' providers and command
//! argument completions exchange them; `packages/tui/src/autocomplete.ts` in
//! pi `v1.0.0`.

use serde::{Deserialize, Serialize};

/// pi-tui's `AutocompleteItem`. An item without a label shows its value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "ItemJson")]
pub struct AutocompleteItem {
    /// What the item inserts.
    pub value: String,
    /// What the list shows.
    pub label: String,
    /// Shown next to the label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Deserialize)]
struct ItemJson {
    value: String,
    label: Option<String>,
    description: Option<String>,
}

impl From<ItemJson> for AutocompleteItem {
    fn from(item: ItemJson) -> AutocompleteItem {
        AutocompleteItem {
            label: item.label.unwrap_or_else(|| item.value.clone()),
            value: item.value,
            description: item.description,
        }
    }
}

/// pi-tui's `AutocompleteSuggestions`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutocompleteSuggestions {
    /// The candidates, best first.
    pub items: Vec<AutocompleteItem>,
    /// The text they complete, which ends at the cursor.
    pub prefix: String,
}

/// Editor lines and a cursor whose column counts UTF-16 code units: what
/// pi-tui's autocomplete providers receive and `applyCompletion` returns.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditorState {
    /// The lines.
    pub lines: Vec<String>,
    /// The cursor's line.
    pub cursor_line: usize,
    /// The cursor's column.
    pub cursor_col: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_item_without_a_label_shows_its_value() {
        let item: AutocompleteItem = serde_json::from_str(r#"{"value": "a"}"#).unwrap();
        assert_eq!(item.label, "a");
        assert_eq!(
            crate::json::to_string(&item).unwrap(),
            r#"{"value":"a","label":"a"}"#
        );
    }
}
