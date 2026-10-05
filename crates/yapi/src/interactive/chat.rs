//! Transcript items and how they render.
//!
//! Ports of the message components in
//! `packages/coding-agent/src/modes/interactive/components` in pi `v1.0.0`.
//! Every item renders its own leading blank line, as pi's do.

use ratatui_core::style::Modifier;
use ratatui_core::text::{Line, Span};
use yapi_tui::lines::{self, StyledLine, box_content_width, boxed, styled};
use yapi_tui::markdown::{self, MarkdownOptions, MarkdownTheme};
use yapi_tui::theme::Theme;
use yapi_types::message::{AssistantMessage, ContentBlock, StopReason};

use super::bash_view::BashView;
use super::tools::ToolView;

/// What rendering needs besides the item.
pub struct RenderContext<'a> {
    /// The theme.
    pub theme: &'a Theme,
    /// Markdown styles from the theme.
    pub markdown: &'a MarkdownTheme,
    /// Tool output and summaries are expanded (`ctrl+o`).
    pub expanded: bool,
    /// Thinking blocks are hidden (`ctrl+t`).
    pub hide_thinking: bool,
    /// Horizontal padding of message text (`outputPadding`).
    pub output_pad: usize,
    /// The expand key, as shown in hints.
    pub expand_key: &'a str,
    /// The cancel keys, as shown in hints.
    pub cancel_key: &'a str,
    /// The home directory, shown as `~`.
    pub home: Option<&'a str>,
    /// The label of hidden thinking blocks.
    pub thinking_label: &'a str,
}

/// Draws an item's rows for a width.
pub type Renderer = Box<dyn Fn(usize, &RenderContext<'_>) -> Vec<StyledLine> + Send>;

/// One transcript item.
pub enum Item {
    /// A user prompt.
    User(String),
    /// A `/skill:<name>` invocation: pi's `SkillInvocationMessageComponent`,
    /// then the user's own text when there is any.
    Skill(Box<yapi_core::agent_session::SkillBlock>),
    /// A model response, possibly still streaming.
    Assistant(Box<AssistantMessage>),
    /// A tool call and its result.
    Tool(Box<ToolView>),
    /// A user `!` command.
    Bash(Box<BashView>),
    /// A compaction summary.
    Compaction {
        /// Context tokens before compaction.
        tokens_before: u64,
        /// The summary.
        summary: String,
    },
    /// A branch summary from tree navigation.
    BranchSummary(String),
    /// A dim status line.
    Status(String),
    /// A warning line.
    Warning(String),
    /// An error line.
    Error(String),
    /// Lines rendered elsewhere, with their own spacing.
    Lines(Vec<StyledLine>),
    /// Rows drawn for the current width, as pi re-wraps its text components
    /// when the terminal is resized.
    Render(Renderer),
    /// A custom message an extension shows.
    Custom(Box<CustomView>),
}

/// pi's `CustomMessageComponent`: the message in a labelled box, or the
/// component an extension's message renderer built.
pub struct CustomView {
    /// The message.
    pub message: yapi_types::message::CustomMessage,
    /// What identifies it to the extension.
    pub key: String,
    /// The extension that draws it, when one does.
    pub renderer: Option<std::sync::Arc<dyn yapi_core::extensions::Extension>>,
    /// The renderer's component.
    pub view: Option<super::extension_ui::RemoteView>,
    /// Requests sent, to drop stale answers.
    pub requests: u64,
}

impl CustomView {
    fn render(&self, width: usize, ctx: &RenderContext<'_>) -> Vec<StyledLine> {
        let theme = ctx.theme;
        let mut out = lines::spacer(1);
        if let Some(view) = &self.view {
            out.extend(view.render(width).0);
            return out;
        }
        out.extend(labelled_box(
            format!("[{}]", self.message.custom_type),
            custom_markdown(&self.message.content.text("\n"), width, ctx),
            width,
            theme,
        ));
        out
    }
}

/// Markdown in the custom-message text color, for a custom-message box at
/// `width`.
fn custom_markdown(text: &str, width: usize, ctx: &RenderContext<'_>) -> Vec<StyledLine> {
    markdown::render(
        text,
        box_content_width(width, 1),
        0,
        0,
        ctx.markdown,
        MarkdownOptions {
            text: Some(ctx.theme.fg("customMessageText")),
            ..MarkdownOptions::default()
        },
    )
}

/// A custom-message box: `label` bold, a blank row, then `body`.
fn labelled_box(
    label: String,
    body: Vec<StyledLine>,
    width: usize,
    theme: &Theme,
) -> Vec<StyledLine> {
    let mut rows = vec![
        Line::from(Span::styled(
            label,
            theme.fg("customMessageLabel").add_modifier(Modifier::BOLD),
        )),
        Line::default(),
    ];
    rows.extend(body);
    boxed(rows, width, 1, 1, Some(theme.bg("customMessageBg")))
}

/// pi's compaction and branch summary components: `label` over `expanded`
/// markdown, or over `collapsed` and the expand hint.
fn summary_box(
    label: &str,
    expanded: &str,
    collapsed: &str,
    width: usize,
    ctx: &RenderContext<'_>,
) -> Vec<StyledLine> {
    let theme = ctx.theme;
    let text = theme.fg("customMessageText");
    let body = if ctx.expanded {
        custom_markdown(expanded, width, ctx)
    } else {
        lines::wrap(
            &Line::from(vec![
                Span::styled(format!("{collapsed} ("), text),
                Span::styled(ctx.expand_key.to_owned(), theme.fg("dim")),
                Span::styled(" to expand)", text),
            ]),
            box_content_width(width, 1),
        )
    };
    let mut out = lines::spacer(1);
    out.extend(labelled_box(label.to_owned(), body, width, theme));
    out
}

/// `12345` as `12,345`.
pub fn group_thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::new();
    for (index, c) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// pi's `UserMessageComponent`.
fn user_message(text: &str, width: usize, ctx: &RenderContext<'_>) -> Vec<StyledLine> {
    markdown::render(
        text,
        width,
        ctx.output_pad,
        1,
        ctx.markdown,
        MarkdownOptions {
            text: Some(ctx.theme.fg("userMessageText")),
            background: Some(ctx.theme.bg("userMessageBg")),
            preserve_list_markers: true,
            preserve_backslash_escapes: true,
        },
    )
}

/// The transcript item for a user prompt: a skill invocation or plain text.
pub fn user_item(text: String) -> Item {
    match yapi_core::agent_session::parse_skill_block(&text) {
        Some(block) => Item::Skill(Box::new(block)),
        None => Item::User(text),
    }
}

fn padded_text(text: StyledLine, width: usize, px: usize) -> Vec<StyledLine> {
    lines::text(&[text], width, px, 0, None)
}

impl Item {
    /// Whether the item animates: a started tool call without a result, or a
    /// running `!` command.
    pub fn animating(&self) -> bool {
        match self {
            Item::Tool(view) => view.result.is_none() && view.started.is_some(),
            Item::Bash(view) => view.running(),
            _ => false,
        }
    }

    /// The item's rows at `width`. `first` says nothing precedes it in the chat.
    pub fn render(&self, width: usize, first: bool, ctx: &RenderContext<'_>) -> Vec<StyledLine> {
        let theme = ctx.theme;
        match self {
            Item::User(text) => {
                let mut out = if first { Vec::new() } else { lines::spacer(1) };
                out.extend(user_message(text, width, ctx));
                out
            }
            Item::Skill(block) => {
                let mut out = if first { Vec::new() } else { lines::spacer(1) };
                let label = theme.fg("customMessageLabel");
                let inner = box_content_width(width, 1);
                let body = if ctx.expanded {
                    let mut body = vec![Line::from(Span::styled(
                        "[skill]",
                        label.add_modifier(Modifier::BOLD),
                    ))];
                    body.extend(custom_markdown(
                        &format!("**{}**\n\n{}", block.name, block.content),
                        width,
                        ctx,
                    ));
                    body
                } else {
                    lines::wrap(
                        &Line::from(vec![
                            Span::styled("[skill]", label.add_modifier(Modifier::BOLD)),
                            Span::styled(" ", label),
                            Span::styled(block.name.clone(), theme.fg("customMessageText")),
                            Span::styled(
                                format!(" ({} to expand)", ctx.expand_key),
                                theme.fg("dim"),
                            ),
                        ]),
                        inner,
                    )
                };
                out.extend(boxed(body, width, 1, 1, Some(theme.bg("customMessageBg"))));
                if let Some(message) = &block.user_message {
                    out.extend(lines::spacer(1));
                    out.extend(user_message(message, width, ctx));
                }
                out
            }
            Item::Assistant(message) => render_assistant(message, width, ctx),
            Item::Tool(tool) => tool.render(width, ctx),
            Item::Custom(custom) => custom.render(width, ctx),
            Item::Bash(bash) => bash.render(width, ctx),
            Item::Compaction {
                tokens_before,
                summary,
            } => {
                let tokens = group_thousands(*tokens_before);
                summary_box(
                    "[compaction]",
                    &format!("**Compacted from {tokens} tokens**\n\n{summary}"),
                    &format!("Compacted from {tokens} tokens"),
                    width,
                    ctx,
                )
            }
            Item::BranchSummary(summary) => summary_box(
                "[branch]",
                &format!("**Branch Summary**\n\n{summary}"),
                "Branch summary",
                width,
                ctx,
            ),
            Item::Status(text) => {
                let mut out = lines::spacer(1);
                out.extend(padded_text(styled(text.clone(), theme.fg("dim")), width, 1));
                out
            }
            Item::Warning(text) => {
                let mut out = lines::spacer(1);
                out.extend(padded_text(
                    styled(format!("Warning: {text}"), theme.fg("warning")),
                    width,
                    1,
                ));
                out
            }
            Item::Error(text) => {
                let mut out = lines::spacer(1);
                out.extend(padded_text(
                    styled(format!("Error: {text}"), theme.fg("error")),
                    width,
                    ctx.output_pad,
                ));
                out
            }
            Item::Lines(lines) => lines.clone(),
            Item::Render(render) => render(width, ctx),
        }
    }
}

fn has_visible(text: &str) -> bool {
    !text.trim().is_empty()
}

/// The message an aborted response shows.
pub fn aborted_message(message: &AssistantMessage) -> String {
    match message.error_message.as_deref() {
        Some(error) if error != "Request was aborted" => error.to_owned(),
        _ => "Operation aborted".to_owned(),
    }
}

fn render_assistant(
    message: &AssistantMessage,
    width: usize,
    ctx: &RenderContext<'_>,
) -> Vec<StyledLine> {
    let theme = ctx.theme;
    let pad = ctx.output_pad;
    let mut out = Vec::new();
    let visible = message.content.iter().any(|block| match block {
        ContentBlock::Text(text) => has_visible(&text.text),
        ContentBlock::Thinking(thinking) => has_visible(&thinking.thinking),
        _ => false,
    });
    if visible {
        out.extend(lines::spacer(1));
    }
    let blocks = &message.content;
    let mut index = 0;
    while index < blocks.len() {
        match &blocks[index] {
            ContentBlock::Text(text) if has_visible(&text.text) => {
                out.extend(markdown::render(
                    text.text.trim(),
                    width,
                    pad,
                    0,
                    ctx.markdown,
                    MarkdownOptions::default(),
                ));
                index += 1;
            }
            ContentBlock::Thinking(_) => {
                let mut parts = Vec::new();
                while let Some(ContentBlock::Thinking(thinking)) = blocks.get(index) {
                    let trimmed = thinking.thinking.trim();
                    if !trimmed.is_empty() {
                        parts.push(trimmed.to_owned());
                    }
                    index += 1;
                }
                if parts.is_empty() {
                    continue;
                }
                if ctx.hide_thinking {
                    out.extend(padded_text(
                        styled(
                            ctx.thinking_label.to_owned(),
                            theme.fg("thinkingText").add_modifier(Modifier::ITALIC),
                        ),
                        width,
                        pad,
                    ));
                } else {
                    out.extend(markdown::render(
                        &parts.join("\n\n"),
                        width,
                        pad,
                        0,
                        ctx.markdown,
                        MarkdownOptions {
                            text: Some(theme.fg("thinkingText").add_modifier(Modifier::ITALIC)),
                            ..MarkdownOptions::default()
                        },
                    ));
                }
                let more = blocks[index..].iter().any(|block| match block {
                    ContentBlock::Text(text) => has_visible(&text.text),
                    ContentBlock::Thinking(thinking) => has_visible(&thinking.thinking),
                    _ => false,
                });
                if more {
                    out.extend(lines::spacer(1));
                }
            }
            _ => index += 1,
        }
    }
    let has_tool_calls = blocks
        .iter()
        .any(|block| matches!(block, ContentBlock::ToolCall(_)));
    let error_line = |text: String| padded_text(styled(text, theme.fg("error")), width, pad);
    if message.stop_reason == StopReason::Length {
        out.extend(lines::spacer(1));
        out.extend(error_line(
            "Response was truncated before completion.".into(),
        ));
    }
    if !has_tool_calls {
        match message.stop_reason {
            StopReason::Aborted => {
                out.extend(lines::spacer(1));
                out.extend(error_line(aborted_message(message)));
            }
            StopReason::Error => {
                out.extend(lines::spacer(1));
                let error = message.error_message.as_deref().unwrap_or("Unknown error");
                out.extend(error_line(format!("Error: {error}")));
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_thousands() {
        assert_eq!(group_thousands(12), "12");
        assert_eq!(group_thousands(12345), "12,345");
        assert_eq!(group_thousands(1234567), "1,234,567");
    }
}
