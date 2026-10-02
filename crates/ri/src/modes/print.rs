//! Print mode: run the prompts, then print the final answer (text) or every event
//! (JSON). Port of `modes/print-mode.ts` in pi `v1.0.0`.

use std::io::Write;
use std::sync::{Arc, Mutex};

use ri_core::agent_session::failed;
use ri_core::extensions::{Mode, NoUi};
use ri_types::message::ContentBlock;

use crate::startup::Startup;

/// Writes one line to stdout, ignoring a closed pipe.
fn write_line(stdout: &Mutex<std::io::Stdout>, line: &str) {
    if let Ok(stdout) = stdout.lock() {
        let mut lock = stdout.lock();
        let _ = lock.write_all(line.as_bytes());
        let _ = lock.write_all(b"\n");
        let _ = lock.flush();
    }
}

/// Runs print mode; returns the process exit code.
pub async fn run(startup: Startup, json: bool) -> u8 {
    let Startup {
        session,
        initial_message,
        initial_images,
        messages,
    } = startup;
    let stdout = Arc::new(Mutex::new(std::io::stdout()));
    let mode = if json { Mode::Json } else { Mode::Print };
    session.bind_extensions(Arc::new(NoUi), mode).await;
    if json {
        if let Some(header) = session.header_json() {
            write_line(&stdout, &header);
        }
        let out = stdout.clone();
        session.subscribe(Box::new(move |event| {
            if let Ok(line) = ri_types::json::to_string(event) {
                write_line(&out, &line);
            }
        }));
    }

    let mut prompts = Vec::new();
    if let Some(message) = initial_message {
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
        && let ri_types::message::Message::Assistant(assistant) = last
    {
        if failed(assistant) {
            let reason = match assistant.stop_reason {
                ri_types::message::StopReason::Aborted => "aborted",
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
                write_line(&stdout, &text.text);
            }
        }
    }
    0
}
