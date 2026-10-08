//! `/status [warn|error] message` adds a status message to the session,
//! which a message renderer draws in a colored box: green for information,
//! yellow for warnings and red for errors.
//!
//! A port of pi's `message-renderer.ts`. pi shows the message's local time
//! when expanded, and this port its UTC time.

use yapi_extension_api::widgets::tui::lines::{box_content_width, boxed, text};
use yapi_extension_api::widgets::{bg_style, parse, to_ansi};
use yapi_extension_api::{Api, Component, json, request, theme};

/// pi-tui's `Box` with padding `pad` around a `Text`, over the custom
/// message background.
struct StatusBox {
    text: String,
    pad: usize,
}

impl Component for StatusBox {
    fn render(&mut self, width: usize) -> Vec<String> {
        let inner = text(
            &[parse(&self.text)],
            box_content_width(width, self.pad),
            0,
            0,
            None,
        );
        let rows = boxed(inner, width, self.pad, 1, Some(bg_style("customMessageBg")));
        to_ansi(&rows, None)
    }
}

fn init(api: &mut Api) {
    api.register_message_renderer("status-update", |message, options| {
        let th = theme();
        let details = &message["details"];
        let level = details["level"].as_str().unwrap_or("info");
        let color = match level {
            "error" => "error",
            "warn" => "warning",
            _ => "success",
        };
        let prefix = th.fg(color, &format!("[{}]", level.to_uppercase()));
        let content = message["content"].as_str().unwrap_or_default();
        let mut text = format!("{prefix} {content}");
        if options["expanded"] == true
            && let Some(timestamp) = details["timestamp"].as_u64()
        {
            let seconds = timestamp / 1000 % 86_400;
            let time = format!(
                "{:02}:{:02}:{:02}",
                seconds / 3600,
                seconds / 60 % 60,
                seconds % 60
            );
            text += &format!("\n{}", th.fg("dim", &format!("  at {time}")));
        }
        let pad = options["outputPad"].as_u64().unwrap_or(1) as usize;
        Some(Box::new(StatusBox { text, pad }))
    });
    api.register_command(
        "status",
        "Send a status message (usage: /status [warn|error] message)",
        |args, _ctx| async move {
            let args = args.trim();
            let mut parts = args.split_whitespace();
            let (level, content) = match parts.next() {
                Some(level @ ("warn" | "error")) => {
                    let rest: Vec<&str> = parts.collect();
                    let content = if rest.is_empty() {
                        "Status update".to_owned()
                    } else {
                        rest.join(" ")
                    };
                    (level, content)
                }
                _ => ("info", args.to_owned()),
            };
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |now| now.as_millis() as u64);
            request(
                "session.sendMessage",
                &json!({
                    "message": {
                        "customType": "status-update",
                        "content": content,
                        "display": true,
                        "details": {"level": level, "timestamp": timestamp},
                    },
                    "options": {},
                }),
            )
            .map(|_| ())
        },
    );
}

yapi_extension_api::extension!(init);
