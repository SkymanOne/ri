//! Print mode: run the prompts, then print the final answer (text) or every event
//! (JSON). Port of `modes/print-mode.ts` in pi `v1.0.0`.

use std::io::Write;
use std::sync::Arc;

use yapi_core::agent_session::failed;
use yapi_core::extensions::{Mode, NoUi};
use yapi_types::message::ContentBlock;

use crate::startup::Startup;

/// Writes one line to stdout, ignoring a closed pipe.
fn write_line(line: &str) {
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(line.as_bytes());
    let _ = stdout.write_all(b"\n");
    let _ = stdout.flush();
}

/// Runs print mode; returns the process exit code.
pub async fn run(startup: Startup, json: bool) -> u8 {
    let Startup {
        session,
        initial_message,
        initial_images,
        messages,
        ..
    } = startup;
    let mode = if json { Mode::Json } else { Mode::Print };
    session.bind_extensions(Arc::new(NoUi), mode, None).await;
    if json {
        if let Some(header) = session.header_json() {
            write_line(&header);
        }
        session.subscribe(Box::new(|event| {
            if let Ok(line) = yapi_types::json::to_string(event) {
                write_line(&line);
            }
        }));
    }

    let mut prompts = Vec::new();
    // pi skips an empty first message, images included.
    if let Some(message) = initial_message.filter(|message| !message.is_empty()) {
        prompts.push((message, initial_images));
    }
    prompts.extend(messages.into_iter().map(|message| (message, Vec::new())));
    for (message, images) in prompts {
        if let Err(error) = session.prompt(&message, images).await {
            eprintln!("{error}");
            session.shutdown().await;
            return 1;
        }
    }
    session.shutdown().await;

    if !json
        && let Some(last) = session.messages().last()
        && let yapi_types::message::Message::Assistant(assistant) = last
    {
        if failed(assistant) {
            let reason = match assistant.stop_reason {
                yapi_types::message::StopReason::Aborted => "aborted",
                _ => "error",
            };
            eprintln!(
                "{}",
                assistant
                    .error_message
                    .clone()
                    .unwrap_or_else(|| format!("Request {reason}"))
            );
            return 1;
        }
        for block in &assistant.content {
            if let ContentBlock::Text(text) = block {
                write_line(&text.text);
            }
        }
    }
    0
}
