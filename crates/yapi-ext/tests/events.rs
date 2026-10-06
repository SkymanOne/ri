//! Extension events whose results change what the session does, driven by
//! a pi extension and the faux provider.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers; a panic is a test failure"
)]

mod common;

use std::path::Path;
use std::sync::Arc;

use common::{cli_source, custom_entries, engine, options, scratch, session};
use serde_json::{Value, json};
use yapi_ai::faux::{Faux, Response};
use yapi_core::agent_session::{AgentSession, TreeNavigation};
use yapi_core::extensions::{Mode, NoUi};
use yapi_ext::ExtensionHost;
use yapi_types::message::{Message, blocks_text};
use yapi_types::session::FileEntry;

/// Records the events it sees, and answers them as `/mode` says, until
/// `/dump` appends what it saw as `seen` entries.
const EXTENSION: &str = r#"
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function (pi: ExtensionAPI) {
	let mode = "supply";
	const seen: unknown[] = [];
	pi.registerCommand("mode", { description: "Sets the answers", handler: async (args) => { mode = args; } });
	pi.registerCommand("dump", {
		description: "Records what was seen",
		handler: async () => {
			for (const event of seen.splice(0)) pi.appendEntry("seen", event);
		},
	});
	pi.on("session_before_compact", (event) => {
		seen.push({
			type: event.type,
			reason: event.reason,
			willRetry: event.willRetry,
			instructions: event.customInstructions ?? null,
			summarize: event.preparation.messagesToSummarize.length,
			branch: event.branchEntries.length > 0,
			settings: event.preparation.settings.keepRecentTokens,
		});
		if (mode === "cancel") return { cancel: true };
		return {
			compaction: {
				summary: "From the extension",
				firstKeptEntryId: event.preparation.firstKeptEntryId,
				tokensBefore: event.preparation.tokensBefore,
				details: { custom: true },
			},
		};
	});
	pi.on("session_compact", (event) => {
		seen.push({ type: event.type, fromExtension: event.fromExtension, summary: event.compactionEntry.summary, reason: event.reason });
	});
	pi.on("session_compact_failed", (event) => {
		seen.push({ type: event.type, aborted: event.aborted, error: event.errorMessage ?? null, fromExtension: event.fromExtension });
	});
	pi.on("session_before_tree", (event) => {
		const { preparation } = event;
		seen.push({ type: event.type, wants: preparation.userWantsSummary, entries: preparation.entriesToSummarize.length, label: preparation.label ?? null });
		if (mode === "cancel") return { cancel: true };
		return { summary: { summary: "From the extension", details: { custom: true } }, label: "extension label" };
	});
	pi.on("session_tree", (event) => {
		seen.push({ type: event.type, fromExtension: event.fromExtension ?? null, summary: event.summaryEntry?.summary ?? null });
	});
}
"#;

async fn extensions(dir: &Path, source: &str) -> Arc<ExtensionHost> {
    let path = dir.join("events.ts");
    std::fs::write(&path, source).unwrap();
    let host = ExtensionHost::load(&engine(), options(dir), &[cli_source(&path)])
        .await
        .unwrap();
    assert!(host.errors().is_empty(), "{:?}", host.errors());
    host
}

/// Runs extension command `text`, which has finished when this returns.
async fn command(session: &AgentSession, text: &str) {
    session.prompt(text, Vec::new()).await.unwrap();
}

/// The events the extension recorded.
async fn seen(session: &AgentSession) -> Vec<Value> {
    command(session, "/dump").await;
    custom_entries(session, "seen")
}

/// A session with two exchanges, which compacts with a small budget.
async fn conversation(faux: &Faux, dir: &Path, host: &Arc<ExtensionHost>) -> AgentSession {
    let session = session(faux, dir, host.for_session());
    session
        .set_nested_global_setting("compaction", "keepRecentTokens", 2.into())
        .unwrap();
    session.bind_extensions(Arc::new(NoUi), Mode::Print).await;
    session.prompt("first", Vec::new()).await.unwrap();
    session.prompt("second", Vec::new()).await.unwrap();
    session
}

#[tokio::test(flavor = "multi_thread")]
async fn compaction_takes_the_summary_an_extension_supplies() {
    let dir = scratch("events-compact");
    let host = extensions(&dir, EXTENSION).await;
    let faux = Faux::new([Response::text("one"), Response::text("two")]);
    let session = conversation(&faux, &dir, &host).await;

    let result = session.compact(Some("focus")).await.unwrap();

    assert_eq!(result.summary, "From the extension");
    assert_eq!(result.details, Some(json!({"custom": true})));
    // No summary request reached the provider.
    assert_eq!(faux.requests().len(), 2);
    let from_hook = session.with_session(|file| {
        file.entries().find_map(|entry| match entry {
            FileEntry::Compaction(compaction) => compaction.from_hook,
            _ => None,
        })
    });
    assert_eq!(from_hook, Some(true));
    assert_eq!(
        seen(&session).await,
        [
            json!({"type": "session_before_compact", "reason": "manual", "willRetry": false, "instructions": "focus", "summarize": 2, "branch": true, "settings": 2}),
            json!({"type": "session_compact", "fromExtension": true, "summary": "From the extension", "reason": "manual"}),
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_extension_cancels_compaction() {
    let dir = scratch("events-compact-cancel");
    let host = extensions(&dir, EXTENSION).await;
    let faux = Faux::new([Response::text("one"), Response::text("two")]);
    let session = conversation(&faux, &dir, &host).await;
    command(&session, "/mode cancel").await;

    let error = session.compact(None).await.unwrap_err();

    assert_eq!(error, "Compaction cancelled");
    assert!(session.with_session(|file| {
        !file
            .entries()
            .any(|entry| matches!(entry, FileEntry::Compaction(_)))
    }));
    let seen = seen(&session).await;
    assert_eq!(
        seen[1],
        json!({"type": "session_compact_failed", "aborted": true, "error": null, "fromExtension": false})
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn tree_navigation_takes_an_extension_summary_or_stops() {
    let dir = scratch("events-tree");
    let host = extensions(&dir, EXTENSION).await;
    let faux = Faux::new([Response::text("one"), Response::text("two")]);
    let session = conversation(&faux, &dir, &host).await;
    let first = session.with_session(|file| {
        file.entries()
            .find_map(|entry| match entry {
                FileEntry::Message(message) if matches!(message.message, Message::Assistant(_)) => {
                    Some(message.meta.id.clone())
                }
                _ => None,
            })
            .unwrap()
    });

    let summarize = TreeNavigation {
        summarize: true,
        ..TreeNavigation::default()
    };
    let outcome = session
        .navigate_tree(&first, summarize.clone())
        .await
        .unwrap();
    let Some(FileEntry::BranchSummary(summary)) = &outcome.summary_entry else {
        panic!("expected a branch summary, got {outcome:?}");
    };
    assert_eq!(summary.summary, "From the extension");
    assert_eq!(summary.from_hook, Some(true));
    assert_eq!(
        session.with_session(|file| file.label(&summary.meta.id).map(str::to_owned)),
        Some("extension label".to_owned())
    );
    assert_eq!(faux.requests().len(), 2);

    command(&session, "/mode cancel").await;
    let leaf = session.with_session(|file| file.leaf_id().map(str::to_owned));
    let outcome = session.navigate_tree(&first, summarize).await.unwrap();
    assert!(outcome.cancelled);
    assert_eq!(
        session.with_session(|file| file.leaf_id().map(str::to_owned)),
        leaf
    );
    let seen = seen(&session).await;
    assert_eq!(
        seen,
        [
            json!({"type": "session_before_tree", "wants": true, "entries": 2, "label": null}),
            json!({"type": "session_tree", "fromExtension": true, "summary": "From the extension"}),
            json!({"type": "session_before_tree", "wants": true, "entries": 2, "label": null}),
        ]
    );
}

/// Replaces every finished assistant message.
const REWRITE: &str = r#"
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function (pi: ExtensionAPI) {
	pi.on("message_end", (event) => {
		const { message } = event;
		if (message.role === "assistant") {
			return { message: { ...message, content: [{ type: "text", text: "replaced" }] } };
		}
	});
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn message_end_replacements_are_recorded_and_sent() {
    let dir = scratch("events-message-end");
    let host = extensions(&dir, REWRITE).await;
    let faux = Faux::new([Response::text("one"), Response::text("two")]);
    let session = session(&faux, &dir, host.for_session());
    session.bind_extensions(Arc::new(NoUi), Mode::Print).await;
    session.prompt("first", Vec::new()).await.unwrap();
    session.prompt("second", Vec::new()).await.unwrap();

    let assistant_text = |message: &Message| match message {
        Message::Assistant(assistant) => Some(blocks_text(&assistant.content, "")),
        _ => None,
    };
    let recorded: Vec<String> = session.with_session(|file| {
        file.entries()
            .filter_map(|entry| match entry {
                FileEntry::Message(entry) => assistant_text(&entry.message),
                _ => None,
            })
            .collect()
    });
    assert_eq!(recorded, ["replaced", "replaced"]);
    let sent: Vec<String> = faux.requests()[1]
        .iter()
        .filter_map(assistant_text)
        .collect();
    assert_eq!(sent, ["replaced"]);
}
