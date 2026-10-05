//! Agent session behavior driven by the faux provider.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers; a panic is a test failure"
)]

use std::path::Path;
use std::sync::Arc;

use yapi_ai::api::Apis;
use yapi_ai::faux::{Faux, Response};
use yapi_ai::registry::ModelRegistry;
use yapi_core::agent_session::{AgentSession, Resources, SessionConfig, TreeNavigation};
use yapi_core::session::SessionManager;
use yapi_core::settings::SettingsManager;
use yapi_types::message::{Message, ThinkingLevel};
use yapi_types::model::Model;
use yapi_types::session::FileEntry;

fn faux_model() -> Model {
    serde_json::from_value(serde_json::json!({
        "id": "faux-1", "name": "Faux", "api": "faux", "provider": "faux", "baseUrl": "",
        "reasoning": false, "input": ["text"],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
        "contextWindow": 100000, "maxTokens": 8000,
    }))
    .unwrap()
}

fn session(faux: &Faux) -> AgentSession {
    let model = faux_model();
    let mut registry = ModelRegistry::builtin();
    registry.register_provider("faux", vec![model.clone()]);
    registry.set_runtime_key("faux", "key".into());
    let mut apis = Apis::default();
    apis.register(Arc::new(faux.clone()));
    AgentSession::new(SessionConfig {
        cwd: Path::new("/work").to_path_buf(),
        agent_dir: Path::new("/agent").to_path_buf(),
        settings: SettingsManager::in_memory(),
        registry,
        apis,
        session: SessionManager::in_memory(Path::new("/work")),
        model: Some(model),
        thinking_level: ThinkingLevel::Off,
        tools: Vec::new(),
        extensions: Vec::new(),
        include_extension_tools: false,
        allowed_tools: None,
        excluded_tools: Vec::new(),
        resources: Resources::default(),
    })
}

fn entry_id(session: &AgentSession, text: &str) -> String {
    session.with_session(|file| {
        file.entries()
            .find_map(|entry| match entry {
                FileEntry::Message(message) => match &message.message {
                    Message::Assistant(assistant)
                        if yapi_types::message::blocks_text(&assistant.content, "") == text =>
                    {
                        Some(message.meta.id.clone())
                    }
                    _ => None,
                },
                _ => None,
            })
            .unwrap()
    })
}

#[tokio::test]
async fn navigating_with_a_summary_records_the_abandoned_branch() {
    let faux = Faux::new([
        Response::text("one"),
        Response::text("two"),
        Response::text("## Goal\nExplore."),
    ]);
    let session = session(&faux);
    session.prompt("first", Vec::new()).await.unwrap();
    session.prompt("second", Vec::new()).await.unwrap();
    let target = entry_id(&session, "one");

    let outcome = session
        .navigate_tree(
            &target,
            TreeNavigation {
                summarize: true,
                label: Some("checkpoint".into()),
                ..TreeNavigation::default()
            },
        )
        .await
        .unwrap();

    let Some(FileEntry::BranchSummary(summary)) = &outcome.summary_entry else {
        panic!("expected a branch summary, got {outcome:?}");
    };
    assert!(summary.summary.starts_with(
        "The user explored a different conversation branch before returning here.\nSummary of that exploration:\n\n## Goal\nExplore."
    ));
    assert_eq!(summary.meta.parent_id.as_deref(), Some(target.as_str()));
    let prompt = faux.requests().last().unwrap().clone();
    let text = match &prompt[1] {
        Message::User(user) => match &user.content {
            yapi_types::message::Content::Blocks(blocks) => {
                yapi_types::message::blocks_text(blocks, "")
            }
            yapi_types::message::Content::Text(text) => text.clone(),
        },
        other => panic!("expected the summary prompt, got {other:?}"),
    };
    assert!(
        text.starts_with("<conversation>\n[User]: second\n\n[Assistant]: two\n</conversation>")
    );
    let roles: Vec<&str> = session
        .messages()
        .iter()
        .map(|message| match message {
            Message::System(_) => "system",
            Message::User(_) => "user",
            Message::Assistant(_) => "assistant",
            Message::BranchSummary(_) => "branchSummary",
            _ => "other",
        })
        .collect();
    assert_eq!(roles, ["system", "user", "assistant", "branchSummary"]);
    let id = summary.meta.id.clone();
    assert_eq!(
        session.with_session(|file| file.label(&id).map(str::to_owned)),
        Some("checkpoint".into())
    );
}

#[tokio::test]
async fn navigating_to_a_user_message_returns_its_text() {
    let faux = Faux::new([Response::text("one")]);
    let session = session(&faux);
    session.prompt("first", Vec::new()).await.unwrap();
    let user = session.with_session(|file| {
        file.entries()
            .find_map(|entry| match entry {
                FileEntry::Message(message) if matches!(message.message, Message::User(_)) => {
                    Some(message.meta.id.clone())
                }
                _ => None,
            })
            .unwrap()
    });
    let outcome = session
        .navigate_tree(&user, TreeNavigation::default())
        .await
        .unwrap();
    assert_eq!(outcome.editor_text.as_deref(), Some("first"));
    assert!(outcome.summary_entry.is_none());
    assert!(
        !session
            .messages()
            .iter()
            .any(|message| matches!(message, Message::User(_)))
    );
}
