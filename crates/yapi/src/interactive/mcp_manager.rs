//! The `/mcp` manager in the editor's place, showing the screens the MCP
//! extension sends. Port of `McpManagerView` in
//! `packages/coding-agent/src/extensions/mcp/ui.ts` in pi `v1.0.0`.

use ratatui_core::style::Modifier;
use ratatui_core::text::{Line, Span};
use tokio::sync::{oneshot, watch};
use yapi_core::mcp::extension::{McpMenu, McpScreen};
use yapi_tui::lines::{self, StyledLine, styled};
use yapi_tui::select_list::{SelectEvent, SelectList, SelectListLayout};
use yapi_tui::text_input::TextInput;
use yapi_tui::theme::Theme;

use super::selectors::{Ui, select_list_theme};

const MAX_VISIBLE_ITEMS: usize = 12;

type Reply = Option<oneshot::Sender<Option<String>>>;

/// The manager.
pub enum McpManager {
    /// A menu, the value of its highlighted item, and its answer until given.
    Menu(watch::Receiver<McpMenu>, Option<String>, Reply),
    /// A title and a message.
    Status(String, String),
    /// A title, the authorization URL, the field and its answer until given.
    RedirectUrl(String, String, Box<TextInput>, Reply),
}

/// Answers once.
fn finish(reply: &mut Reply, value: Option<String>) {
    if let Some(reply) = reply.take() {
        let _ = reply.send(value);
    }
}

/// The current menu and its list, with the highlighted item kept by value as
/// the menu changes.
fn menu_list(
    menus: &watch::Receiver<McpMenu>,
    selected: &mut Option<String>,
    theme: &Theme,
) -> (McpMenu, SelectList) {
    let menu = menus.borrow().clone();
    let mut list = SelectList::new(
        menu.items.iter().cloned().map(Into::into).collect(),
        menu.items.len().min(MAX_VISIBLE_ITEMS),
        select_list_theme(theme),
        SelectListLayout::default(),
    );
    let wanted = selected.as_ref().or(menu.selected.as_ref());
    if let Some(index) = menu
        .items
        .iter()
        .position(|item| Some(&item.value) == wanted)
    {
        list.set_selected_index(index);
    }
    *selected = list.selected_item().map(|item| item.value.clone());
    (menu, list)
}

impl McpManager {
    /// The manager showing `screen`; `None` for [`McpScreen::Close`].
    pub fn new(screen: McpScreen) -> Option<McpManager> {
        Some(match screen {
            McpScreen::Menu(menus, reply) => McpManager::Menu(menus, None, Some(reply)),
            McpScreen::Status(title, message) => McpManager::Status(title, message),
            McpScreen::RedirectUrl(title, url, reply) => {
                let mut input = TextInput::default();
                input.focused = true;
                McpManager::RedirectUrl(title, url, Box::new(input), Some(reply))
            }
            McpScreen::Close => return None,
        })
    }

    /// pi's `frame`: the title and `body` between borders, with key hints.
    pub fn render(
        &mut self,
        width: usize,
        ui: &Ui<'_>,
    ) -> (Vec<StyledLine>, Option<(usize, usize)>) {
        let theme = ui.theme;
        let text = |content: &str, color: &str| {
            lines::text(&[styled(content, theme.fg(color))], width, 1, 0, None)
        };
        let hints = |confirm: Option<&str>, cancel: &str| {
            let mut spans = Vec::new();
            if let Some(confirm) = confirm {
                spans.extend(ui.key_hint("tui.select.confirm", confirm));
                spans.push(Span::raw(" • "));
            }
            spans.extend(ui.key_hint("tui.select.cancel", cancel));
            Some(spans)
        };
        let mut body = Vec::new();
        let mut cursor = None;
        let (title, footer) = match self {
            McpManager::Menu(menus, selected, _) => {
                let (menu, list) = menu_list(menus, selected, theme);
                for (content, color) in [(&menu.details, "muted"), (&menu.error, "error")] {
                    if let Some(content) = content {
                        body.extend(text(content, color));
                    }
                }
                body.extend(lines::spacer(1));
                let footer = if menu.items.is_empty() {
                    body.extend(text(&menu.empty, "muted"));
                    hints(None, &menu.cancel_label)
                } else {
                    body.extend(list.render(width));
                    hints(Some(&menu.confirm_label), &menu.cancel_label)
                };
                (menu.title, footer)
            }
            McpManager::Status(title, message) => {
                body.extend(lines::spacer(1));
                body.extend(text(message, "muted"));
                (title.clone(), None)
            }
            McpManager::RedirectUrl(title, url, input, _) => {
                // pi links the URL and the hint; the terminal detects the URL.
                let click = if cfg!(target_os = "macos") {
                    "Cmd+click to open"
                } else {
                    "Ctrl+click to open"
                };
                body.extend(lines::spacer(1));
                body.extend(text(
                    "Approve access in your browser. If it did not open, visit:",
                    "muted",
                ));
                body.extend(text(url, "accent"));
                body.extend(text(click, "dim"));
                body.extend(lines::spacer(1));
                body.extend(text(
                    "If the browser runs on another machine, paste the URL it was redirected to:",
                    "muted",
                ));
                cursor = input.cursor_column().map(|column| (body.len(), column));
                body.push(input.render(width));
                (title.clone(), hints(Some("submit"), "cancel"))
            }
        };
        let border = lines::border(width, theme.fg("accent"));
        let mut out = vec![border.clone()];
        let title = styled(title, theme.fg("accent").add_modifier(Modifier::BOLD));
        out.extend(lines::text(&[title], width, 1, 0, None));
        let cursor = cursor.map(|(row, column)| (row + out.len(), column));
        out.extend(body);
        if let Some(footer) = footer {
            out.extend(lines::spacer(1));
            out.extend(lines::text(&[Line::from(footer)], width, 1, 0, None));
        }
        out.push(border);
        (out, cursor)
    }

    /// Handles a key.
    pub fn handle_input(&mut self, data: &str, ui: &Ui<'_>) {
        let keys = ui.keys;
        match self {
            McpManager::Menu(menus, selected, reply) => {
                let (menu, mut list) = menu_list(menus, selected, ui.theme);
                if menu.items.is_empty() {
                    if keys.matches(data, "tui.select.cancel") {
                        finish(reply, None);
                    }
                    return;
                }
                match list.handle_input(data, keys) {
                    SelectEvent::Moved => {
                        *selected = list.selected_item().map(|item| item.value.clone());
                    }
                    SelectEvent::Selected(item) => finish(reply, Some(item.value)),
                    SelectEvent::Cancelled => finish(reply, None),
                    SelectEvent::Ignored => {}
                }
            }
            McpManager::Status(..) => {}
            McpManager::RedirectUrl(_, _, input, reply) => {
                if keys.matches(data, "tui.select.confirm") {
                    let value = input.value().trim();
                    if !value.is_empty() {
                        let value = value.to_owned();
                        finish(reply, Some(value));
                    }
                } else if keys.matches(data, "tui.select.cancel") {
                    finish(reply, None);
                } else {
                    input.handle_input(data, keys);
                }
            }
        }
    }
}
