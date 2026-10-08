//! A `question` tool that asks the user to pick one of the model's options
//! in a component, or to type an answer of their own in an editor, and
//! draws its call and result in the transcript.
//!
//! A port of pi's `question.ts`.

use yapi_extension_api::widgets::tui::editor::{Editor, EditorEvent, EditorTheme};
use yapi_extension_api::widgets::tui::select_list::SelectListTheme;
use yapi_extension_api::widgets::{
    Text, keybindings, style, to_ansi, visible_width, wrap_text_with_ansi,
};
use yapi_extension_api::{
    Api, Component, CustomOptions, Done, Tool, ToolResult, Value, json, parse_key, theme,
};

/// The answer: its text, whether the user typed it, and the option's
/// number.
type Answer = Option<(String, bool, Option<usize>)>;

struct Choice {
    label: String,
    description: Option<String>,
    other: bool,
}

struct Question {
    question: String,
    options: Vec<Choice>,
    index: usize,
    editing: bool,
    editor: Editor,
    done: Done<Answer>,
}

impl Question {
    /// `text` wrapped after `prefix`, the rows after the first indented.
    fn wrapped(lines: &mut Vec<String>, width: usize, prefix: &str, text: &str) {
        let prefix_width = visible_width(prefix);
        if prefix_width >= width {
            lines.extend(wrap_text_with_ansi(&format!("{prefix}{text}"), width));
            return;
        }
        let indent = " ".repeat(prefix_width);
        for (row, line) in wrap_text_with_ansi(text, width - prefix_width)
            .into_iter()
            .enumerate()
        {
            lines.push(format!("{}{line}", if row == 0 { prefix } else { &indent }));
        }
    }
}

impl Component for Question {
    fn handle_input(&mut self, data: &str) {
        let key = parse_key(data);
        if self.editing {
            if key.as_deref() == Some("escape") {
                self.editing = false;
                self.editor.set_text("");
            } else if let EditorEvent::Submit(text) = self.editor.handle_input(data, &keybindings())
            {
                let text = text.trim();
                if text.is_empty() {
                    self.editing = false;
                    self.editor.set_text("");
                } else {
                    self.done.finish(Some((text.to_owned(), true, None)));
                }
            }
            return;
        }
        match key.as_deref() {
            Some("up") => self.index = self.index.saturating_sub(1),
            Some("down") => self.index = (self.index + 1).min(self.options.len() - 1),
            Some("enter") if self.options[self.index].other => self.editing = true,
            Some("enter") => {
                let label = self.options[self.index].label.clone();
                self.done.finish(Some((label, false, Some(self.index + 1))));
            }
            Some("escape") => self.done.finish(None),
            _ => {}
        }
    }

    fn render(&mut self, width: usize) -> Vec<String> {
        let th = theme();
        let width = width.max(1);
        let rule = th.fg("accent", &"─".repeat(width));
        let mut lines = vec![rule.clone()];
        Self::wrapped(&mut lines, width, " ", &th.fg("text", &self.question));
        lines.push(String::new());
        for (index, option) in self.options.iter().enumerate() {
            let selected = index == self.index;
            let prefix = if selected {
                th.fg("accent", "> ")
            } else {
                "  ".to_owned()
            };
            let pencil = if option.other && self.editing {
                " ✎"
            } else {
                ""
            };
            let label = format!("{}. {}{pencil}", index + 1, option.label);
            let color = if selected || (option.other && self.editing) {
                "accent"
            } else {
                "text"
            };
            Self::wrapped(&mut lines, width, &prefix, &th.fg(color, &label));
            if let Some(description) = &option.description {
                Self::wrapped(&mut lines, width, "     ", &th.fg("muted", description));
            }
        }
        if self.editing {
            lines.push(String::new());
            Self::wrapped(&mut lines, width, " ", &th.fg("muted", "Your answer:"));
            let rows = self.editor.render(width.saturating_sub(2).max(1));
            lines.extend(
                to_ansi(&rows, None)
                    .into_iter()
                    .map(|row| format!(" {row}")),
            );
        }
        lines.push(String::new());
        let help = if self.editing {
            "Enter to submit • Esc to go back"
        } else {
            "↑↓ navigate • Enter to select • Esc to cancel"
        };
        Self::wrapped(&mut lines, width, " ", &th.fg("dim", help));
        lines.push(rule);
        lines
    }
}

fn parameters() -> Value {
    json!({
        "type": "object",
        "required": ["question", "options"],
        "properties": {
            "question": {"type": "string", "description": "The question to ask the user"},
            "options": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["label"],
                    "properties": {
                        "label": {"type": "string", "description": "Display label for the option"},
                        "description": {"type": "string", "description": "Optional description shown below label"},
                    },
                },
                "description": "Options for the user to choose from",
            },
        },
    })
}

fn result(
    text: &str,
    question: &Value,
    options: &[&str],
    answer: Value,
    custom: Option<bool>,
) -> ToolResult {
    let mut details = json!({"question": question, "options": options, "answer": answer});
    if let Some(custom) = custom {
        details["wasCustom"] = json!(custom);
    }
    ToolResult::text(text).with_details(details)
}

fn init(api: &mut Api) {
    let tool = Tool::new(
        "question",
        "Ask the user a question and let them pick from options. Use when you need user input to proceed.",
        parameters(),
        |params, ctx| async move {
            let question = &params["question"];
            let given = params["options"].as_array().cloned().unwrap_or_default();
            let labels: Vec<&str> = given.iter().map(|o| o["label"].as_str().unwrap_or_default()).collect();
            if ctx.mode() != "tui" {
                let text = "Error: UI not available (running in non-interactive mode)";
                return Ok(result(text, question, &labels, Value::Null, None));
            }
            if given.is_empty() {
                return Ok(result("Error: No options provided", question, &[], Value::Null, None));
            }
            let mut options: Vec<Choice> = given
                .iter()
                .map(|option| Choice {
                    label: option["label"].as_str().unwrap_or_default().to_owned(),
                    description: option["description"].as_str().map(str::to_owned),
                    other: false,
                })
                .collect();
            options.push(Choice {
                label: "Type something.".into(),
                description: None,
                other: true,
            });
            let accent = style("accent");
            let editor_theme = EditorTheme {
                border: accent,
                select_list: SelectListTheme {
                    selected_text: accent,
                    description: style("muted"),
                    scroll_info: style("dim"),
                    no_match: style("warning"),
                },
            };
            let text = question.as_str().unwrap_or_default().to_owned();
            let answer = ctx
                .custom(
                    |done| Question {
                        question: text,
                        options,
                        index: 0,
                        editing: false,
                        editor: Editor::new(editor_theme, 0, 5),
                        done,
                    },
                    CustomOptions::default(),
                )
                .await
                .flatten();
            Ok(match answer {
                None => result("User cancelled the selection", question, &labels, Value::Null, None),
                Some((answer, true, _)) => {
                    let text = format!("User wrote: {answer}");
                    result(&text, question, &labels, json!(answer), Some(true))
                }
                Some((answer, false, index)) => {
                    let text = format!("User selected: {}. {answer}", index.unwrap_or_default());
                    result(&text, question, &labels, json!(answer), Some(false))
                }
            })
        },
    )
    .label("Question")
    .execution_mode("sequential")
    .render_call(|args, _ctx| {
        let th = theme();
        let question = args["question"].as_str().unwrap_or_default();
        let mut text = th.fg("toolTitle", &th.bold("question ")) + &th.fg("muted", question);
        let options = args["options"].as_array().map(Vec::as_slice).unwrap_or_default();
        if !options.is_empty() {
            let labels = options.iter().map(|o| o["label"].as_str().unwrap_or_default());
            let numbered: Vec<String> = labels
                .chain(["Type something."])
                .enumerate()
                .map(|(index, label)| format!("{}. {label}", index + 1))
                .collect();
            text += &format!("\n{}", th.fg("dim", &format!("  Options: {}", numbered.join(", "))));
        }
        Some(Box::new(Text::new(text, 0, 0)))
    })
    .render_result(|result, _options, _ctx| {
        let th = theme();
        let details = &result["details"];
        let text = if !details.is_object() {
            result["content"][0]["text"].as_str().unwrap_or_default().to_owned()
        } else if let Some(answer) = details["answer"].as_str() {
            if details["wasCustom"] == true {
                th.fg("success", "✓ ") + &th.fg("muted", "(wrote) ") + &th.fg("accent", answer)
            } else {
                let options = details["options"].as_array().map(Vec::as_slice).unwrap_or_default();
                let display = match options.iter().position(|option| option == answer) {
                    Some(index) => format!("{}. {answer}", index + 1),
                    None => answer.to_owned(),
                };
                th.fg("success", "✓ ") + &th.fg("accent", &display)
            }
        } else {
            th.fg("warning", "Cancelled")
        };
        Some(Box::new(Text::new(text, 0, 0)))
    });
    api.register_tool(tool);
}

yapi_extension_api::extension!(init);
