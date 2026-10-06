//! The `/tree` selector: every entry of the session as a navigable tree.
//!
//! Port of `components/tree-selector.ts` in
//! `packages/coding-agent/src/modes/interactive` in pi `v1.0.0`.

use std::collections::{HashMap, HashSet};

use ratatui_core::style::{Modifier, Style};
use ratatui_core::text::{Line, Span};
use serde_json::Value;
use unicode_segmentation::UnicodeSegmentation;
use yapi_core::session::{SessionTree, TreeNode};
use yapi_tui::lines::{self, StyledLine, styled};
use yapi_tui::select_list::{step, visible_range};
use yapi_tui::text::grapheme_width;
use yapi_tui::text_input::TextInput;
use yapi_types::message::{Content, ContentBlock, Message, StopReason, blocks_text};
use yapi_types::session::FileEntry;
use yapi_types::settings::TreeFilterMode as Filter;

use super::keybindings::key_text;
use super::selectors::{Action, Outcome, Ui};

const GUTTER_WIDTH: usize = 2;

const CYCLE: [Filter; 5] = [
    Filter::Default,
    Filter::NoTools,
    Filter::UserOnly,
    Filter::LabeledOnly,
    Filter::All,
];

/// A node waiting to be laid out, with what its parent decided for it.
struct Walk<T> {
    node: T,
    indent: usize,
    just_branched: bool,
    connector: bool,
    last: bool,
    gutters: Vec<(usize, bool)>,
    virtual_root: bool,
}

impl<T> Walk<T> {
    /// The walks of `roots`, the first root last so that it pops first;
    /// `multiple` says the tree shows several roots.
    fn roots(roots: Vec<T>, multiple: bool) -> Vec<Walk<T>> {
        let count = roots.len();
        roots
            .into_iter()
            .enumerate()
            .rev()
            .map(|(position, node)| Walk {
                node,
                indent: usize::from(multiple),
                just_branched: multiple,
                connector: multiple,
                last: position + 1 == count,
                gutters: Vec::new(),
                virtual_root: multiple,
            })
            .collect()
    }

    /// Pushes the walks of this node's `children` onto `stack`, the first
    /// child on top.
    fn push_children(self, children: Vec<T>, multiple: bool, stack: &mut Vec<Walk<T>>) {
        let branching = children.len() > 1;
        let indent = if branching || (self.just_branched && self.indent > 0) {
            self.indent + 1
        } else {
            self.indent
        };
        let mut gutters = self.gutters;
        if self.connector && !self.virtual_root {
            let display = if multiple {
                self.indent.saturating_sub(1)
            } else {
                self.indent
            };
            gutters.push((display.saturating_sub(1), !self.last));
        }
        let count = children.len();
        for (position, node) in children.into_iter().enumerate().rev() {
            stack.push(Walk {
                node,
                indent,
                just_branched: branching,
                connector: branching,
                last: position + 1 == count,
                gutters: gutters.clone(),
                virtual_root: false,
            });
        }
    }
}

#[derive(Clone, Debug)]
struct Flat {
    node: usize,
    indent: usize,
    connector: bool,
    last: bool,
    gutters: Vec<(usize, bool)>,
    virtual_root_child: bool,
}

fn entry_id(entry: &FileEntry) -> &str {
    entry.meta().map_or("", |meta| meta.id.as_str())
}

fn parent_id(entry: &FileEntry) -> Option<&str> {
    entry.meta().and_then(|meta| meta.parent_id.as_deref())
}

fn role(message: &Message) -> &'static str {
    match message {
        Message::System(_) => "system",
        Message::User(_) => "user",
        Message::Assistant(_) => "assistant",
        Message::ToolResult(_) => "toolResult",
        Message::BashExecution(_) => "bashExecution",
        Message::Custom(_) => "custom",
        Message::BranchSummary(_) => "branchSummary",
        Message::CompactionSummary(_) => "compactionSummary",
    }
}

/// A message's text, as pi's `extractFullContent` reads it.
fn message_text(message: &Message) -> Option<String> {
    Some(match message {
        Message::System(system) => system.content.text(""),
        Message::User(user) => user.content.text(""),
        Message::Assistant(assistant) => blocks_text(&assistant.content, ""),
        Message::ToolResult(result) => blocks_text(&result.content, ""),
        Message::Custom(custom) => custom.content.text(""),
        _ => return None,
    })
}

fn normalize(text: &str) -> String {
    text.replace(['\n', '\t'], " ").trim().to_owned()
}

fn has_text(message: &Message) -> bool {
    match message {
        Message::Assistant(assistant) => assistant.content.iter().any(|block| match block {
            ContentBlock::Text(text) => !text.text.trim().is_empty(),
            _ => false,
        }),
        _ => message_text(message).is_some_and(|text| !text.trim().is_empty()),
    }
}

fn is_settings_entry(entry: &FileEntry) -> bool {
    matches!(
        entry,
        FileEntry::Label(_)
            | FileEntry::ContextEdit(_)
            | FileEntry::Custom(_)
            | FileEntry::ModelChange(_)
            | FileEntry::ThinkingLevelChange(_)
            | FileEntry::SessionInfo(_)
    )
}

/// Slices a styled line to the columns `start..start + len`, dropping
/// characters that straddle either edge.
fn slice_columns(line: &Line<'_>, start: usize, len: usize) -> StyledLine {
    let mut column = 0;
    let mut spans = Vec::new();
    for span in &line.spans {
        let mut text = String::new();
        for grapheme in span.content.graphemes(true) {
            let width = grapheme_width(grapheme);
            if column >= start && column + width <= start + len {
                text.push_str(grapheme);
            }
            column += width;
        }
        if !text.is_empty() {
            spans.push(Span::styled(text, span.style));
        }
    }
    Line::from(spans)
}

/// pi's key-name shortening in tree help: whole words only.
fn replace_words(text: &str) -> String {
    let mut out = String::new();
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        out.push_str(match word.as_str() {
            "pageUp" => "pgup",
            "pageDown" => "pgdn",
            "up" => "↑",
            "down" => "↓",
            "left" => "←",
            "right" => "→",
            other => other,
        });
        word.clear();
    };
    for c in text.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            word.push(c);
        } else {
            flush(&mut word, &mut out);
            out.push(c);
        }
    }
    flush(&mut word, &mut out);
    out
}

/// The `/tree` selector.
pub struct TreeSelector {
    tree: SessionTree,
    by_id: HashMap<String, usize>,
    flat: Vec<Flat>,
    filtered: Vec<Flat>,
    selected: usize,
    leaf: Option<String>,
    max_visible: usize,
    filter: Filter,
    search: String,
    tool_calls: HashMap<String, (String, Value)>,
    multiple_roots: bool,
    show_label_times: bool,
    active_path: HashSet<String>,
    visible_parent: HashMap<String, Option<String>>,
    visible_children: HashMap<Option<String>, Vec<String>>,
    last_selected: Option<String>,
    folded: HashSet<String>,
    label_input: Option<(String, TextInput)>,
    home: Option<String>,
}

impl TreeSelector {
    /// A selector over `tree` with `leaf` as the current position, showing
    /// `max(5, rows / 2)` entries.
    pub fn new(
        tree: SessionTree,
        leaf: Option<String>,
        terminal_rows: usize,
        initial: Option<String>,
        filter: Filter,
        home: Option<String>,
    ) -> TreeSelector {
        let by_id = tree
            .nodes
            .iter()
            .enumerate()
            .map(|(index, node)| (entry_id(&node.entry).to_owned(), index))
            .collect();
        let mut selector = TreeSelector {
            multiple_roots: tree.roots.len() > 1,
            tree,
            by_id,
            flat: Vec::new(),
            filtered: Vec::new(),
            selected: 0,
            leaf,
            max_visible: (terminal_rows / 2).max(5),
            filter,
            search: String::new(),
            tool_calls: HashMap::new(),
            show_label_times: false,
            active_path: HashSet::new(),
            visible_parent: HashMap::new(),
            visible_children: HashMap::new(),
            last_selected: None,
            folded: HashSet::new(),
            label_input: None,
            home,
        };
        selector.flatten();
        selector.build_active_path();
        selector.apply_filter();
        let target = initial.or_else(|| selector.leaf.clone());
        selector.selected = selector.nearest_visible(target.as_deref());
        selector.last_selected = selector.selected_id();
        selector
    }

    fn node(&self, index: usize) -> &TreeNode {
        &self.tree.nodes[index]
    }

    fn id(&self, index: usize) -> &str {
        entry_id(&self.tree.nodes[index].entry)
    }

    /// The selected entry's id.
    fn selected_id(&self) -> Option<String> {
        self.filtered
            .get(self.selected)
            .map(|flat| self.id(flat.node).to_owned())
    }

    /// The id of the entry above `id`.
    fn parent_of(&self, id: &str) -> Option<String> {
        let &node = self.by_id.get(id)?;
        parent_id(&self.node(node).entry).map(str::to_owned)
    }

    fn nearest_visible(&self, id: Option<&str>) -> usize {
        if self.filtered.is_empty() {
            return 0;
        }
        let visible: HashMap<&str, usize> = self
            .filtered
            .iter()
            .enumerate()
            .map(|(position, flat)| (self.id(flat.node), position))
            .collect();
        std::iter::successors(id.map(str::to_owned), |id| self.parent_of(id))
            .find_map(|id| visible.get(id.as_str()).copied())
            .unwrap_or(self.filtered.len() - 1)
    }

    fn build_active_path(&mut self) {
        let path = std::iter::successors(self.leaf.clone(), |id| self.parent_of(id)).collect();
        self.active_path = path;
    }

    /// pi's `flattenTree`: depth-first, the branch holding the leaf first.
    fn flatten(&mut self) {
        self.tool_calls.clear();
        let count = self.tree.nodes.len();
        let mut contains_active = vec![false; count];
        let mut order = Vec::with_capacity(count);
        let mut stack: Vec<usize> = self.tree.roots.iter().rev().copied().collect();
        while let Some(node) = stack.pop() {
            order.push(node);
            stack.extend(self.tree.nodes[node].children.iter().rev().copied());
        }
        for &node in order.iter().rev() {
            let mut has = self.leaf.as_deref() == Some(self.id(node));
            for &child in &self.tree.nodes[node].children {
                has |= contains_active[child];
            }
            contains_active[node] = has;
        }
        let mut roots = self.tree.roots.clone();
        roots.sort_by_key(|root| !contains_active[*root]);
        let mut stack = Walk::roots(roots, self.multiple_roots);
        let mut result = Vec::new();
        while let Some(walk) = stack.pop() {
            let node = walk.node;
            if let FileEntry::Message(entry) = &self.tree.nodes[node].entry
                && let Message::Assistant(assistant) = &entry.message
            {
                for block in &assistant.content {
                    if let ContentBlock::ToolCall(call) = block {
                        self.tool_calls.insert(
                            call.id.clone(),
                            (call.name.clone(), Value::Object(call.arguments.clone())),
                        );
                    }
                }
            }
            result.push(Flat {
                node,
                indent: walk.indent,
                connector: walk.connector,
                last: walk.last,
                gutters: walk.gutters.clone(),
                virtual_root_child: walk.virtual_root,
            });
            let children = &self.tree.nodes[node].children;
            let mut ordered: Vec<usize> = children
                .iter()
                .copied()
                .filter(|child| contains_active[*child])
                .collect();
            ordered.extend(
                children
                    .iter()
                    .copied()
                    .filter(|child| !contains_active[*child]),
            );
            walk.push_children(ordered, self.multiple_roots, &mut stack);
        }
        self.flat = result;
    }

    fn searchable(&self, node: &TreeNode) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(label) = &node.label {
            parts.push(label.clone());
        }
        match &node.entry {
            FileEntry::Message(entry) => {
                parts.push(role(&entry.message).to_owned());
                if let Some(text) = message_text(&entry.message) {
                    parts.push(yapi_types::js::slice(&text, 0, 200));
                }
                if let Message::BashExecution(bash) = &entry.message {
                    parts.push(bash.command.clone());
                }
            }
            FileEntry::CustomMessage(entry) => {
                parts.push(entry.custom_type.clone());
                parts.push(match &entry.content {
                    Content::Text(text) => text.clone(),
                    Content::Blocks(blocks) => {
                        yapi_types::js::slice(&blocks_text(blocks, ""), 0, 200)
                    }
                });
            }
            FileEntry::Compaction(_) => parts.push("compaction".into()),
            FileEntry::BranchSummary(entry) => {
                parts.push("branch summary".into());
                parts.push(entry.summary.clone());
            }
            FileEntry::SessionInfo(entry) => {
                parts.push("title".into());
                if let Some(name) = entry.name.as_ref().filter(|name| !name.is_empty()) {
                    parts.push(name.clone());
                }
            }
            FileEntry::ModelChange(entry) => {
                parts.push("model".into());
                parts.push(entry.model_id.clone());
            }
            FileEntry::ThinkingLevelChange(entry) => {
                parts.push("thinking".into());
                parts.push(entry.thinking_level.clone());
            }
            FileEntry::Custom(entry) => {
                parts.push("custom".into());
                parts.push(entry.custom_type.clone());
            }
            FileEntry::ContextEdit(entry) => {
                parts.push("context edit".into());
                parts.push(
                    if entry.replacement.is_none() {
                        "omit"
                    } else {
                        "replace"
                    }
                    .into(),
                );
                parts.push(entry.target_id.clone());
            }
            FileEntry::Label(entry) => {
                parts.push("label".into());
                parts.push(entry.label.clone().unwrap_or_default());
            }
            _ => {}
        }
        parts.join(" ")
    }

    fn apply_filter(&mut self) {
        if let Some(id) = self.selected_id() {
            self.last_selected = Some(id);
        }
        let tokens: Vec<String> = self
            .search
            .to_lowercase()
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        let filtered: Vec<Flat> = self
            .flat
            .iter()
            .filter(|flat| {
                let node = self.node(flat.node);
                let entry = &node.entry;
                if matches!(entry, FileEntry::Usage(_)) {
                    return false;
                }
                let current = self.leaf.as_deref() == Some(entry_id(entry));
                if let FileEntry::Message(message) = entry
                    && let Message::Assistant(assistant) = &message.message
                    && !current
                {
                    let failed = !matches!(
                        assistant.stop_reason,
                        StopReason::Stop | StopReason::ToolUse
                    );
                    if !has_text(&message.message) && !failed {
                        return false;
                    }
                }
                let is_user = matches!(entry, FileEntry::Message(message) if matches!(message.message, Message::User(_)));
                let passes = match self.filter {
                    Filter::UserOnly => is_user,
                    Filter::NoTools => {
                        !is_settings_entry(entry)
                            && !matches!(entry, FileEntry::Message(message) if matches!(message.message, Message::ToolResult(_)))
                    }
                    Filter::LabeledOnly => node.label.is_some(),
                    Filter::All => true,
                    Filter::Default => !is_settings_entry(entry),
                };
                if !passes {
                    return false;
                }
                if tokens.is_empty() {
                    return true;
                }
                let text = self.searchable(node).to_lowercase();
                tokens.iter().all(|token| text.contains(token.as_str()))
            })
            .cloned()
            .collect();
        self.filtered = filtered;
        if !self.folded.is_empty() {
            let mut skip: HashSet<String> = HashSet::new();
            for flat in &self.flat {
                let entry = &self.node(flat.node).entry;
                if let Some(parent) = parent_id(entry)
                    && (self.folded.contains(parent) || skip.contains(parent))
                {
                    skip.insert(entry_id(entry).to_owned());
                }
            }
            let tree = &self.tree;
            self.filtered
                .retain(|flat| !skip.contains(entry_id(&tree.nodes[flat.node].entry)));
        }
        self.recalculate();
        if let Some(last) = self.last_selected.clone() {
            self.selected = self.nearest_visible(Some(&last));
        } else if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len().saturating_sub(1);
        }
        if let Some(id) = self.selected_id() {
            self.last_selected = Some(id);
        }
    }

    /// pi's `recalculateVisualStructure`: hidden entries' children attach to
    /// the nearest visible ancestor.
    fn recalculate(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        let visible: HashSet<String> = self
            .filtered
            .iter()
            .map(|flat| self.id(flat.node).to_owned())
            .collect();
        let visible_ancestor = |id: &str| {
            std::iter::successors(self.parent_of(id), |id| self.parent_of(id))
                .find(|id| visible.contains(id))
        };
        let mut parents: HashMap<String, Option<String>> = HashMap::new();
        let mut children: HashMap<Option<String>, Vec<String>> = HashMap::new();
        children.insert(None, Vec::new());
        for flat in &self.filtered {
            let id = self.id(flat.node).to_owned();
            let ancestor = visible_ancestor(&id);
            parents.insert(id.clone(), ancestor.clone());
            children.entry(ancestor).or_default().push(id);
        }
        let roots = children.get(&None).cloned().unwrap_or_default();
        self.multiple_roots = roots.len() > 1;
        let positions: HashMap<String, usize> = self
            .filtered
            .iter()
            .enumerate()
            .map(|(position, flat)| (self.id(flat.node).to_owned(), position))
            .collect();
        let mut stack = Walk::roots(roots, self.multiple_roots);
        while let Some(walk) = stack.pop() {
            let Some(&position) = positions.get(&walk.node) else {
                continue;
            };
            let flat = &mut self.filtered[position];
            flat.indent = walk.indent;
            flat.connector = walk.connector;
            flat.last = walk.last;
            flat.gutters = walk.gutters.clone();
            flat.virtual_root_child = walk.virtual_root;
            let kids = children
                .get(&Some(walk.node.clone()))
                .cloned()
                .unwrap_or_default();
            walk.push_children(kids, self.multiple_roots, &mut stack);
        }
        self.visible_parent = parents;
        self.visible_children = children;
    }

    fn foldable(&self, id: &str) -> bool {
        let has_children = self
            .visible_children
            .get(&Some(id.to_owned()))
            .is_some_and(|children| !children.is_empty());
        if !has_children {
            return false;
        }
        match self.visible_parent.get(id) {
            None | Some(None) => true,
            Some(Some(parent)) => self
                .visible_children
                .get(&Some(parent.clone()))
                .is_some_and(|siblings| siblings.len() > 1),
        }
    }

    fn segment_start(&self, down: bool) -> usize {
        let Some(flat) = self.filtered.get(self.selected) else {
            return self.selected;
        };
        let positions: HashMap<&str, usize> = self
            .filtered
            .iter()
            .enumerate()
            .map(|(position, flat)| (self.id(flat.node), position))
            .collect();
        let position = |id: &str| positions.get(id).copied().unwrap_or(self.selected);
        let mut current = self.id(flat.node).to_owned();
        if down {
            loop {
                let children = self
                    .visible_children
                    .get(&Some(current.clone()))
                    .cloned()
                    .unwrap_or_default();
                match children.len() {
                    0 => return position(&current),
                    1 => current = children[0].clone(),
                    _ => return position(&children[0]),
                }
            }
        }
        loop {
            let Some(Some(parent)) = self.visible_parent.get(&current).cloned() else {
                return position(&current);
            };
            let siblings = self
                .visible_children
                .get(&Some(parent.clone()))
                .map_or(0, Vec::len);
            if siblings > 1 {
                let start = position(&current);
                if start < self.selected {
                    return start;
                }
            }
            current = parent;
        }
    }

    fn shorten(&self, path: &str) -> String {
        super::tools::shorten_home(path, self.home.as_deref())
    }

    /// pi's `formatToolCall`.
    fn format_tool_call(&self, name: &str, args: &Value) -> String {
        let text = |value: &Value| match value {
            Value::String(text) => text.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        };
        let field = |key: &str| {
            args.get(key)
                .filter(|value| !value.is_null() && *value != "")
        };
        let path = || {
            field("path")
                .or_else(|| field("file_path"))
                .map(text)
                .unwrap_or_default()
        };
        match name {
            "read" => {
                let mut display = self.shorten(&path());
                let offset = args.get("offset").and_then(Value::as_f64);
                let limit = args.get("limit").and_then(Value::as_f64);
                if offset.is_some() || limit.is_some() {
                    let start = offset.unwrap_or(1.0);
                    display += &format!(":{}", yapi_core::tools::js_number(start));
                    if let Some(limit) = limit {
                        let end = start + limit - 1.0;
                        if end != 0.0 {
                            display += &format!("-{}", yapi_core::tools::js_number(end));
                        }
                    }
                }
                format!("[read: {display}]")
            }
            "write" => format!("[write: {}]", self.shorten(&path())),
            "edit" => format!("[edit: {}]", self.shorten(&path())),
            "bash" => {
                let raw = field("command").map(text).unwrap_or_default();
                let command = normalize(&raw);
                let cut: String = command.chars().take(50).collect();
                let more = if yapi_types::js::len(&raw) > 50 {
                    "..."
                } else {
                    ""
                };
                format!("[bash: {cut}{more}]")
            }
            "grep" => format!(
                "[grep: /{}/ in {}]",
                field("pattern").map(text).unwrap_or_default(),
                self.shorten(&field("path").map_or_else(|| ".".to_owned(), text))
            ),
            "find" => format!(
                "[find: {} in {}]",
                field("pattern").map(text).unwrap_or_default(),
                self.shorten(&field("path").map_or_else(|| ".".to_owned(), text))
            ),
            "ls" => format!(
                "[ls: {}]",
                self.shorten(&field("path").map_or_else(|| ".".to_owned(), text))
            ),
            _ => {
                let json = yapi_types::json::stringify(args);
                let cut: String = json.chars().take(40).collect();
                let more = if yapi_types::js::len(&json) > 40 {
                    "..."
                } else {
                    ""
                };
                format!("[{name}: {cut}{more}]")
            }
        }
    }

    fn display(&self, node: &TreeNode, selected: bool, ui: &Ui<'_>) -> Vec<Span<'static>> {
        let theme = ui.theme;
        let dim = theme.fg("dim");
        let muted = theme.fg("muted");
        let mut spans: Vec<Span<'static>> = match &node.entry {
            FileEntry::Message(entry) => match &entry.message {
                Message::User(user) => vec![
                    Span::styled("user: ", theme.fg("accent")),
                    Span::raw(normalize(&yapi_types::js::slice(
                        &user.content.text(""),
                        0,
                        200,
                    ))),
                ],
                Message::Assistant(assistant) => {
                    let label = Span::styled("assistant: ", theme.fg("success"));
                    let text = normalize(&yapi_types::js::slice(
                        &blocks_text(&assistant.content, ""),
                        0,
                        200,
                    ));
                    if !text.is_empty() {
                        vec![label, Span::raw(text)]
                    } else if assistant.stop_reason == StopReason::Aborted {
                        vec![label, Span::styled("(aborted)", muted)]
                    } else if let Some(error) = &assistant.error_message {
                        let error: String = normalize(error).chars().take(80).collect();
                        vec![label, Span::styled(error, theme.fg("error"))]
                    } else {
                        vec![label, Span::styled("(no content)", muted)]
                    }
                }
                Message::ToolResult(result) => {
                    let text = match self.tool_calls.get(&result.tool_call_id) {
                        Some((name, args)) => self.format_tool_call(name, args),
                        None => format!("[{}]", result.tool_name),
                    };
                    vec![Span::styled(text, muted)]
                }
                Message::BashExecution(bash) => {
                    vec![Span::styled(
                        format!("[bash]: {}", normalize(&bash.command)),
                        dim,
                    )]
                }
                other => vec![Span::styled(format!("[{}]", role(other)), dim)],
            },
            FileEntry::CustomMessage(entry) => vec![
                Span::styled(
                    format!("[{}]: ", entry.custom_type),
                    theme.fg("customMessageLabel"),
                ),
                Span::raw(normalize(&entry.content.text(""))),
            ],
            FileEntry::Compaction(entry) => vec![Span::styled(
                format!(
                    "[compaction: {}k tokens]",
                    (entry.tokens_before as f64 / 1000.0).round()
                ),
                theme.fg("borderAccent"),
            )],
            FileEntry::BranchSummary(entry) => vec![
                Span::styled("[branch summary]: ", theme.fg("warning")),
                Span::raw(normalize(&entry.summary)),
            ],
            FileEntry::ModelChange(entry) => {
                vec![Span::styled(format!("[model: {}]", entry.model_id), dim)]
            }
            FileEntry::ThinkingLevelChange(entry) => vec![Span::styled(
                format!("[thinking: {}]", entry.thinking_level),
                dim,
            )],
            FileEntry::Custom(entry) => {
                vec![Span::styled(
                    format!("[custom: {}]", entry.custom_type),
                    dim,
                )]
            }
            FileEntry::ContextEdit(entry) => vec![Span::styled(
                format!(
                    "[context {}: {}]",
                    if entry.replacement.is_none() {
                        "omit"
                    } else {
                        "replace"
                    },
                    entry.target_id
                ),
                dim,
            )],
            FileEntry::Label(entry) => vec![Span::styled(
                format!("[label: {}]", entry.label.as_deref().unwrap_or("(cleared)")),
                dim,
            )],
            FileEntry::SessionInfo(entry) => {
                match entry.name.as_ref().filter(|name| !name.is_empty()) {
                    Some(name) => vec![
                        Span::styled("[title: ", dim),
                        Span::styled(name.clone(), dim),
                        Span::styled("]", dim),
                    ],
                    None => vec![
                        Span::styled("[title: ", dim),
                        Span::styled("empty", dim.add_modifier(Modifier::ITALIC)),
                        Span::styled("]", dim),
                    ],
                }
            }
            _ => Vec::new(),
        };
        if selected {
            for span in &mut spans {
                span.style = span.style.add_modifier(Modifier::BOLD);
            }
        }
        spans
    }

    fn status_labels(&self) -> String {
        let mut labels = match self.filter {
            Filter::NoTools => " [no-tools]",
            Filter::UserOnly => " [user]",
            Filter::LabeledOnly => " [labeled]",
            Filter::All => " [all]",
            Filter::Default => "",
        }
        .to_owned();
        if self.show_label_times {
            labels.push_str(" [+label time]");
        }
        labels
    }

    /// pi formats label times in local time; yapi shows UTC.
    fn label_time(timestamp: &str) -> String {
        let Some(ms) = yapi_core::time::parse_iso(timestamp) else {
            return String::new();
        };
        let iso = yapi_core::time::iso(ms);
        let now = yapi_core::time::now_iso();
        let (date, time) = (&iso[..10], &iso[11..16]);
        if date == &now[..10] {
            return time.to_owned();
        }
        let month: u32 = iso[5..7].parse().unwrap_or(0);
        let day: u32 = iso[8..10].parse().unwrap_or(0);
        if iso[..4] == now[..4] {
            return format!("{month}/{day} {time}");
        }
        format!("{}/{month}/{day} {time}", &iso[2..4])
    }

    fn list(&self, width: usize, ui: &Ui<'_>) -> Vec<StyledLine> {
        let theme = ui.theme;
        let muted = theme.fg("muted");
        if self.filtered.is_empty() {
            return vec![
                lines::truncate(&styled("  No entries found", muted), width, "..."),
                lines::truncate(
                    &styled(format!("  (0/0){}", self.status_labels()), muted),
                    width,
                    "...",
                ),
            ];
        }
        let count = self.filtered.len();
        let (start, end) = visible_range(self.selected, count, self.max_visible);
        let mut rows: Vec<(StyledLine, StyledLine, usize, bool)> = Vec::new();
        for position in start..end {
            let flat = &self.filtered[position];
            let node = self.node(flat.node);
            let id = entry_id(&node.entry);
            let selected = position == self.selected;
            let display_indent = if self.multiple_roots {
                flat.indent.saturating_sub(1)
            } else {
                flat.indent
            };
            let connector = flat.connector && !flat.virtual_root_child;
            let connector_position = if connector {
                display_indent.checked_sub(1)
            } else {
                None
            };
            let folded = self.folded.contains(id);
            let mut prefix = String::new();
            for column in 0..display_indent * 3 {
                let level = column / 3;
                let within = column % 3;
                if let Some((_, show)) = flat.gutters.iter().find(|(at, _)| *at == level) {
                    prefix.push(if within == 0 && *show { '│' } else { ' ' });
                } else if connector_position == Some(level) {
                    prefix.push(match within {
                        0 if flat.last => '└',
                        0 => '├',
                        1 if folded => '⊞',
                        1 if self.foldable(id) => '⊟',
                        1 => '─',
                        _ => ' ',
                    });
                } else {
                    prefix.push(' ');
                }
            }
            let mut body = vec![Span::styled(prefix, theme.fg("dim"))];
            if folded && !connector {
                body.push(Span::styled("⊞ ", theme.fg("accent")));
            }
            if self.active_path.contains(id) {
                body.push(Span::styled("• ", theme.fg("accent")));
            }
            let anchor: usize = body
                .iter()
                .map(|span| lines::width(&Line::from(span.clone())))
                .sum();
            if let Some(label) = &node.label {
                body.push(Span::styled(format!("[{label}] "), theme.fg("warning")));
                if self.show_label_times
                    && let Some(timestamp) = &node.label_timestamp
                {
                    body.push(Span::styled(
                        format!("{} ", Self::label_time(timestamp)),
                        muted,
                    ));
                }
            }
            body.extend(self.display(node, selected, ui));
            let mut gutter = if selected {
                Line::from(Span::styled("› ", theme.fg("accent")))
            } else {
                Line::from(Span::raw("  "))
            };
            let mut body = Line::from(body);
            if selected {
                let bg = theme.bg("selectedBg");
                for span in gutter.spans.iter_mut().chain(body.spans.iter_mut()) {
                    span.style = bg.patch(span.style);
                }
            }
            rows.push((gutter, body, anchor, selected));
        }
        // pi's renderHorizontalViewport: pan only to keep the selected entry's
        // text visible; the gutter stays.
        let viewport = width.saturating_sub(GUTTER_WIDTH);
        let max_body = rows
            .iter()
            .map(|(_, body, _, _)| lines::width(body))
            .max()
            .unwrap_or(0);
        let max_scroll = max_body.saturating_sub(viewport);
        let mut scroll = 0;
        if let Some((_, _, anchor, _)) = rows.iter().find(|(_, _, _, selected)| *selected)
            && max_scroll > 0
        {
            let min_visible = (viewport / 3).clamp(4, 20);
            if *anchor > viewport.saturating_sub(min_visible) {
                let context = (viewport / 4).clamp(2, 12);
                scroll = max_scroll.min(anchor.saturating_sub(context));
            }
        }
        let mut out: Vec<StyledLine> = rows
            .into_iter()
            .map(|(gutter, body, _, _)| {
                let body = if scroll > 0 {
                    slice_columns(&body, scroll, viewport)
                } else {
                    body
                };
                let mut line = gutter;
                line.spans.extend(body.spans);
                lines::truncate(&line, width, "")
            })
            .collect();
        out.push(lines::truncate(
            &styled(
                format!("  ({}/{count}){}", self.selected + 1, self.status_labels()),
                muted,
            ),
            width,
            "...",
        ));
        out
    }

    fn help(&self, width: usize, ui: &Ui<'_>) -> Vec<StyledLine> {
        let first_key = |action: &str| ui.keys.keys(action).first().cloned();
        let format = |actions: &[&str]| -> String {
            let keys: Vec<String> = actions
                .iter()
                .filter_map(|action| first_key(action))
                .collect();
            if keys.is_empty() {
                return String::new();
            }
            let compact = if keys.len() == 1 {
                keys[0].clone()
            } else {
                let parts: Vec<(&str, &str)> = keys
                    .iter()
                    .map(|key| match key.rfind('+') {
                        Some(index) => (&key[..=index], &key[index + 1..]),
                        None => ("", key.as_str()),
                    })
                    .collect();
                let prefix = parts[0].0;
                if !prefix.is_empty() && parts.iter().all(|(other, _)| *other == prefix) {
                    format!(
                        "{prefix}{}",
                        parts
                            .iter()
                            .map(|(_, suffix)| *suffix)
                            .collect::<Vec<_>>()
                            .join("/")
                    )
                } else {
                    keys.join("/")
                }
            };
            let text = compact
                .split('/')
                .map(key_text)
                .collect::<Vec<_>>()
                .join("/");
            replace_words(&text)
        };
        let items: [(&[&str], &str, bool); 8] = [
            (&["tui.select.up", "tui.select.down"], "move", false),
            (
                &["tui.editor.cursorLeft", "tui.editor.cursorRight"],
                "page",
                false,
            ),
            (
                &["app.tree.foldOrUp", "app.tree.unfoldOrDown"],
                "branch",
                false,
            ),
            (&["app.message.copy"], "copy", false),
            (&["app.tree.editLabel"], "label", false),
            (&["app.tree.toggleLabelTimestamp"], "label time", false),
            (
                &[
                    "app.tree.filter.default",
                    "app.tree.filter.noTools",
                    "app.tree.filter.userOnly",
                    "app.tree.filter.labeledOnly",
                    "app.tree.filter.all",
                ],
                "filters",
                true,
            ),
            (
                &[
                    "app.tree.filter.cycleForward",
                    "app.tree.filter.cycleBackward",
                ],
                "cycle",
                true,
            ),
        ];
        let items: Vec<String> = items
            .iter()
            .map(|(actions, label, label_first)| {
                let keys = format(actions);
                if keys.is_empty() {
                    (*label).to_owned()
                } else if *label_first {
                    format!("{label} {keys}")
                } else {
                    format!("{keys} {label}")
                }
            })
            .collect();
        let available = width.max(1);
        let measure = yapi_tui::text::visible_width;
        let mut rows: Vec<String> = Vec::new();
        let mut current = String::new();
        for item in items {
            let indented = format!("  {item}");
            let candidate = if !current.is_empty() {
                format!("{current} · {item}")
            } else if measure(&indented) <= available {
                indented.clone()
            } else {
                item.clone()
            };
            if current.is_empty() || measure(&candidate) <= available {
                current = candidate;
                continue;
            }
            rows.push(current.trim_end().to_owned());
            current = if measure(&indented) <= available {
                indented
            } else {
                item
            };
        }
        if !current.is_empty() {
            rows.push(current.trim_end().to_owned());
        }
        rows.iter()
            .flat_map(|row| lines::wrap(&styled(row.clone(), ui.theme.fg("muted")), available))
            .collect()
    }

    /// The selector's rows and cursor.
    pub fn render(
        &mut self,
        width: usize,
        ui: &Ui<'_>,
    ) -> (Vec<StyledLine>, Option<(usize, usize)>) {
        let theme = ui.theme;
        let mut out = lines::spacer(1);
        out.push(ui.border(width));
        out.extend(lines::text_row(
            styled("  Session Tree", Style::new().add_modifier(Modifier::BOLD)),
            width,
            1,
        ));
        out.extend(self.help(width, ui));
        let mut search = vec![
            Span::raw("  "),
            Span::styled("Type to search:", theme.fg("muted")),
        ];
        if !self.search.is_empty() {
            search.push(Span::raw(" "));
            search.push(Span::styled(self.search.clone(), theme.fg("accent")));
        }
        out.push(lines::truncate(&Line::from(search), width, "..."));
        out.push(ui.border(width));
        out.extend(lines::spacer(1));
        let mut cursor = None;
        match &mut self.label_input {
            Some((_, input)) => {
                let indent = "  ";
                out.push(lines::truncate(
                    &Line::from(vec![
                        Span::raw(indent),
                        Span::styled("Label (empty to remove):", theme.fg("muted")),
                    ]),
                    width,
                    "...",
                ));
                let mut row = input.render(width.saturating_sub(indent.len()));
                cursor = input
                    .cursor_column()
                    .map(|col| (out.len(), col + indent.len()));
                row.spans.insert(0, Span::raw(indent));
                out.push(lines::truncate(&row, width, "..."));
                let mut hint = vec![Span::raw(indent)];
                hint.extend(super::selectors::join_hints(&[
                    ui.key_hint("tui.select.confirm", "save"),
                    ui.key_hint("tui.select.cancel", "cancel"),
                ]));
                out.push(lines::truncate(&Line::from(hint), width, "..."));
            }
            None => out.extend(self.list(width, ui)),
        }
        out.extend(lines::spacer(1));
        out.push(ui.border(width));
        (out, cursor)
    }

    fn copy_text(&self, node: &TreeNode) -> Option<String> {
        let text = match &node.entry {
            FileEntry::Message(entry) => match &entry.message {
                Message::BashExecution(bash) => Some(bash.command.clone()),
                Message::Assistant(assistant) => {
                    let text = blocks_text(&assistant.content, "");
                    if text.is_empty() {
                        assistant.error_message.clone()
                    } else {
                        Some(text)
                    }
                }
                other => message_text(other),
            },
            FileEntry::CustomMessage(entry) => Some(entry.content.text("")),
            FileEntry::Compaction(entry) => Some(entry.summary.clone()),
            FileEntry::BranchSummary(entry) => Some(entry.summary.clone()),
            _ => None,
        };
        text.filter(|text| !text.trim().is_empty())
    }

    fn set_filter(&mut self, filter: Filter) {
        self.filter = filter;
        self.folded.clear();
        self.apply_filter();
    }

    /// Handles a key.
    pub fn handle_input(&mut self, data: &str, ui: &Ui<'_>) -> Outcome {
        let kb = ui.keys;
        if let Some((id, input)) = &mut self.label_input {
            if kb.matches(data, "tui.select.confirm") {
                let value = input.value().trim().to_owned();
                let label = (!value.is_empty()).then_some(value);
                let id = id.clone();
                if let Some(&node) = self.by_id.get(&id) {
                    self.tree.nodes[node].label = label.clone();
                    self.tree.nodes[node].label_timestamp =
                        label.as_ref().map(|_| yapi_core::time::now_iso());
                }
                self.label_input = None;
                return Outcome::Side(Action::Label { id, label });
            }
            if kb.matches(data, "tui.select.cancel") {
                self.label_input = None;
                return Outcome::None;
            }
            // Enter is handled above; a `\n` submit does nothing.
            input.handle_input(data, kb);
            return Outcome::None;
        }
        let count = self.filtered.len();
        let toggled = [
            ("app.tree.filter.default", Filter::Default),
            ("app.tree.filter.noTools", Filter::NoTools),
            ("app.tree.filter.userOnly", Filter::UserOnly),
            ("app.tree.filter.labeledOnly", Filter::LabeledOnly),
            ("app.tree.filter.all", Filter::All),
        ]
        .into_iter()
        .find(|(action, _)| kb.matches(data, action));
        if kb.matches(data, "tui.select.up") {
            self.selected = step(self.selected, count, false);
        } else if kb.matches(data, "tui.select.down") {
            self.selected = step(self.selected, count, true);
        } else if kb.matches(data, "app.tree.foldOrUp") {
            match self.selected_id() {
                Some(id) if self.foldable(&id) && !self.folded.contains(&id) => {
                    self.folded.insert(id);
                    self.apply_filter();
                }
                _ => self.selected = self.segment_start(false),
            }
        } else if kb.matches(data, "app.tree.unfoldOrDown") {
            match self.selected_id() {
                Some(id) if self.folded.contains(&id) => {
                    self.folded.remove(&id);
                    self.apply_filter();
                }
                _ => self.selected = self.segment_start(true),
            }
        } else if kb.matches(data, "tui.editor.cursorLeft") || kb.matches(data, "tui.select.pageUp")
        {
            self.selected = self.selected.saturating_sub(self.max_visible);
        } else if kb.matches(data, "tui.editor.cursorRight")
            || kb.matches(data, "tui.select.pageDown")
        {
            self.selected = (self.selected + self.max_visible).min(count.saturating_sub(1));
        } else if kb.matches(data, "tui.select.confirm") {
            if let Some(id) = self.selected_id() {
                return Outcome::Done(Action::Tree(id));
            }
        } else if kb.matches(data, "app.message.copy") {
            let text = self
                .filtered
                .get(self.selected)
                .and_then(|flat| self.copy_text(self.node(flat.node)));
            return Outcome::Side(Action::Copy(text));
        } else if kb.matches(data, "tui.select.cancel") {
            if self.search.is_empty() {
                return Outcome::Cancel;
            }
            self.search.clear();
            self.folded.clear();
            self.apply_filter();
        } else if let Some((_, filter)) = toggled {
            // A filter's key toggles it, back to the default.
            self.set_filter(if self.filter == filter {
                Filter::Default
            } else {
                filter
            });
        } else if kb.matches(data, "app.tree.filter.cycleBackward")
            || kb.matches(data, "app.tree.filter.cycleForward")
        {
            let forward = kb.matches(data, "app.tree.filter.cycleForward");
            let index = CYCLE
                .iter()
                .position(|filter| *filter == self.filter)
                .unwrap_or(0);
            self.set_filter(CYCLE[step(index, CYCLE.len(), forward)]);
        } else if kb.matches(data, "tui.editor.deleteCharBackward") {
            if !self.search.is_empty() {
                self.search.pop();
                self.folded.clear();
                self.apply_filter();
            }
        } else if kb.matches(data, "app.tree.editLabel") {
            if let Some(flat) = self.filtered.get(self.selected) {
                let node = self.node(flat.node);
                let mut input = TextInput::default();
                input.focused = true;
                if let Some(label) = &node.label {
                    input.set_value(label);
                }
                self.label_input = Some((entry_id(&node.entry).to_owned(), input));
            }
        } else if kb.matches(data, "app.tree.toggleLabelTimestamp") {
            self.show_label_times = !self.show_label_times;
        } else if !data.is_empty() && !data.chars().any(char::is_control) {
            self.search.push_str(data);
            self.folded.clear();
            self.apply_filter();
        }
        Outcome::None
    }
}
