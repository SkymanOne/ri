//! Transcript items and how they render.
//!
//! Ports of the message components in
//! `packages/coding-agent/src/modes/interactive/components` in pi `v1.0.0`.
//! Every item renders its own leading blank line, as pi's do.

use ratatui_core::style::Modifier;
use ratatui_core::text::{Line, Span};
use ri_tui::lines::{self, StyledLine, box_content_width, boxed, styled};
use ri_tui::markdown::{self, MarkdownOptions, MarkdownTheme};
use ri_tui::theme::Theme;
use ri_types::message::{AssistantMessage, ContentBlock, StopReason};

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
}

/// One transcript item.
pub enum Item {
    /// A user prompt.
    User(String),
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

fn padded_text(text: StyledLine, width: usize, px: usize) -> Vec<StyledLine> {
    lines::text(&[text], width, px, 0, None)
}

impl Item {
    /// The item's rows at `width`. `first` says nothing precedes it in the chat.
    pub fn render(&self, width: usize, first: bool, ctx: &RenderContext<'_>) -> Vec<StyledLine> {
        let theme = ctx.theme;
        match self {
            Item::User(text) => {
                let mut out = if first { Vec::new() } else { lines::spacer(1) };
                out.extend(markdown::render(
                    text,
                    width,
                    ctx.output_pad,
                    1,
                    ctx.markdown,
                    MarkdownOptions {
                        text: Some(theme.fg("userMessageText")),
                        background: Some(theme.bg("userMessageBg")),
                        preserve_list_markers: true,
                        preserve_backslash_escapes: true,
                    },
                ));
                out
            }
            Item::Assistant(message) => render_assistant(message, width, ctx),
            Item::Tool(tool) => tool.render(width, ctx),
            Item::Bash(bash) => bash.render(width, ctx),
            Item::Compaction {
                tokens_before,
                summary,
            } => {
                let label = Line::from(Span::styled(
                    "[compaction]",
                    theme.fg("customMessageLabel").add_modifier(Modifier::BOLD),
                ));
                let inner = box_content_width(width, 1);
                let tokens = group_thousands(*tokens_before);
                let mut body = vec![label, Line::default()];
                if ctx.expanded {
                    body.extend(markdown::render(
                        &format!("**Compacted from {tokens} tokens**\n\n{summary}"),
                        inner,
                        0,
                        0,
                        ctx.markdown,
                        MarkdownOptions {
                            text: Some(theme.fg("customMessageText")),
                            ..MarkdownOptions::default()
                        },
                    ));
                } else {
                    body.extend(lines::wrap(
                        &Line::from(vec![
                            Span::styled(
                                format!("Compacted from {tokens} tokens ("),
                                theme.fg("customMessageText"),
                            ),
                            Span::styled(ctx.expand_key.to_owned(), theme.fg("dim")),
                            Span::styled(" to expand)", theme.fg("customMessageText")),
                        ]),
                        inner,
                    ));
                }
                let mut out = lines::spacer(1);
                out.extend(boxed(body, width, 1, 1, Some(theme.bg("customMessageBg"))));
                out
            }
            Item::BranchSummary(summary) => {
                let label = Line::from(Span::styled(
                    "[branch]",
                    theme.fg("customMessageLabel").add_modifier(Modifier::BOLD),
                ));
                let inner = box_content_width(width, 1);
                let mut body = vec![label, Line::default()];
                if ctx.expanded {
                    body.extend(markdown::render(
                        &format!("**Branch Summary**\n\n{summary}"),
                        inner,
                        0,
                        0,
                        ctx.markdown,
                        MarkdownOptions {
                            text: Some(theme.fg("customMessageText")),
                            ..MarkdownOptions::default()
                        },
                    ));
                } else {
                    body.extend(lines::wrap(
                        &Line::from(vec![
                            Span::styled("Branch summary (", theme.fg("customMessageText")),
                            Span::styled(ctx.expand_key.to_owned(), theme.fg("dim")),
                            Span::styled(" to expand)", theme.fg("customMessageText")),
                        ]),
                        inner,
                    ));
                }
                let mut out = lines::spacer(1);
                out.extend(boxed(body, width, 1, 1, Some(theme.bg("customMessageBg"))));
                out
            }
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
                            "Thinking...",
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
