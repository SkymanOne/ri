//! Print mode: run the prompts, then print the final answer (text) or every event
//! (JSON). Port of `modes/print-mode.ts` in pi `v1.0.0`.

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use yapi_core::agent_session::failed;
use yapi_core::extensions::{Mode, NoUi};
use yapi_types::message::ContentBlock;

use crate::runtime::{Bind, Runtime, SessionFactory, forward_newest};
use crate::startup::Startup;

/// Writes one line to stdout, ignoring a closed pipe.
fn write_line(line: &str) {
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(line.as_bytes());
    let _ = stdout.write_all(b"\n");
    let _ = stdout.flush();
}

/// Runs print mode; returns the process exit code. Extension commands
/// replace the session through `factory`, and later prompts go to the
/// replacement.
pub async fn run(startup: Startup, json: bool, factory: SessionFactory) -> u8 {
    let Startup {
        session,
        initial_message,
        initial_images,
        messages,
        ..
    } = startup;
    let mode = if json { Mode::Json } else { Mode::Print };
    if json && let Some(header) = session.header_json() {
        write_line(&header);
    }
    // The current session's number; listeners of older ones stay silent.
    let epoch = Arc::new(AtomicU64::new(0));
    let bind: Bind = Box::new(move |session, replaced| {
        if json {
            forward_newest(&session, &epoch, |event| {
                if let Ok(line) = yapi_types::json::to_string(event) {
                    write_line(&line);
                }
            });
        }
        Box::pin(async move {
            session
                .bind_extensions(Arc::new(NoUi), mode, replaced)
                .await;
        })
    });

    let mut prompts = Vec::new();
    // pi skips an empty first message, images included.
    if let Some(message) = initial_message.filter(|message| !message.is_empty()) {
        prompts.push((message, initial_images));
    }
    prompts.extend(messages.into_iter().map(|message| (message, Vec::new())));
    let local = tokio::task::LocalSet::new();
    let session = local
        .run_until(async move {
            let runtime = Runtime::start(session, factory, bind).await;
            for (message, images) in prompts {
                if let Err(error) = runtime.session().prompt(&message, images).await {
                    eprintln!("{error}");
                    runtime.session().shutdown().await;
                    return None;
                }
            }
            Some(runtime.session())
        })
        .await;
    let Some(session) = session else {
        return 1;
    };
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
