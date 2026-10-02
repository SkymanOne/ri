//! The editor's completion source.

use crate::select_list::SelectItem;

/// Completions for the text before the cursor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Suggestions {
    /// Candidates, best first.
    pub items: Vec<SelectItem>,
    /// The text being completed, such as `/mo` or `@src/ma`.
    pub prefix: String,
}

/// Editor content after applying a completion.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Completion {
    /// The new lines.
    pub lines: Vec<String>,
    /// The cursor line.
    pub cursor_line: usize,
    /// The cursor's byte column.
    pub cursor_col: usize,
}

/// Supplies and applies completions. Cursor columns are byte offsets.
pub trait AutocompleteProvider {
    /// Completions at the cursor; `force` is set for an explicit Tab outside a
    /// trigger context.
    fn suggestions(
        &self,
        lines: &[String],
        line: usize,
        col: usize,
        force: bool,
    ) -> Option<Suggestions>;

    /// The editor content with `item` replacing `prefix` at the cursor.
    fn apply(
        &self,
        lines: &[String],
        line: usize,
        col: usize,
        item: &SelectItem,
        prefix: &str,
    ) -> Completion;

    /// Whether Tab should offer file completion at the cursor.
    fn should_trigger_file_completion(&self, _lines: &[String], _line: usize, _col: usize) -> bool {
        true
    }

    /// Characters besides `@` and `#` that open completion at a token boundary.
    fn trigger_characters(&self) -> Vec<char> {
        Vec::new()
    }
}
