//! Helpers shared by the session tests.
#![allow(
    clippy::unwrap_used,
    dead_code,
    reason = "test helpers; each test file uses some"
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::json;
use yapi_ai::api::Apis;
use yapi_ai::faux::Faux;
use yapi_ai::registry::ModelRegistry;
use yapi_core::agent_session::{AgentSession, Resources, SessionConfig};
use yapi_core::extensions::Extension;
use yapi_core::session::SessionManager;
use yapi_core::settings::SettingsManager;
use yapi_ext::{Engine, Options};
use yapi_types::message::{Content, Message, ThinkingLevel};
use yapi_types::model::Model;
use yapi_types::rpc::SourceInfo;
use yapi_types::session::FileEntry;

pub fn faux_model() -> Model {
    serde_json::from_value(json!({
        "id": "faux-1", "name": "Faux", "api": "faux", "provider": "faux", "baseUrl": "",
        "reasoning": false, "input": ["text"],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
        "contextWindow": 100000, "maxTokens": 8000,
    }))
    .unwrap()
}

pub fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("session-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub fn engine() -> Engine {
    Engine::new(Some(
        &Path::new(env!("CARGO_TARGET_TMPDIR")).join("wasm-cache"),
    ))
    .unwrap()
}

pub fn options(dir: &Path) -> Options {
    let mut options = Options::new(dir.to_path_buf());
    options.agent_dir = dir.join("agent");
    options
}

pub fn cli_source(path: &Path) -> SourceInfo {
    SourceInfo {
        path: path.to_string_lossy().into_owned(),
        source: "cli".into(),
        scope: "temporary".into(),
        origin: "top-level".into(),
        base_dir: None,
    }
}

pub fn session(faux: &Faux, dir: &Path, extensions: Vec<Arc<dyn Extension>>) -> AgentSession {
    session_with_tools(faux, dir, extensions, &["read"])
}

/// [`session`] with these built-in tools active.
pub fn session_with_tools(
    faux: &Faux,
    dir: &Path,
    extensions: Vec<Arc<dyn Extension>>,
    tools: &[&str],
) -> AgentSession {
    let model = faux_model();
    let mut registry = ModelRegistry::builtin();
    registry.register_provider("faux", vec![model.clone()]);
    registry.set_runtime_key("faux", "key".into());
    let mut apis = Apis::default();
    apis.register(Arc::new(faux.clone()));
    AgentSession::new(SessionConfig {
        cwd: dir.to_path_buf(),
        agent_dir: dir.join("agent"),
        settings: SettingsManager::in_memory(),
        registry,
        apis,
        session: SessionManager::in_memory(dir),
        model: Some(model),
        thinking_level: ThinkingLevel::Off,
        tools: tools.iter().map(|tool| (*tool).to_owned()).collect(),
        extensions,
        include_extension_tools: true,
        allowed_tools: None,
        excluded_tools: Vec::new(),
        resources: Resources::default(),
    })
}

pub fn custom_entries(session: &AgentSession, kind: &str) -> Vec<serde_json::Value> {
    session.with_session(|file| {
        file.entries()
            .filter_map(|entry| match entry {
                FileEntry::Custom(custom) if custom.custom_type == kind => custom.data.clone(),
                _ => None,
            })
            .collect()
    })
}

pub fn text_of(message: &Message) -> String {
    match message {
        Message::ToolResult(result) => yapi_types::message::blocks_text(&result.content, "|"),
        Message::Custom(custom) => match &custom.content {
            Content::Text(text) => text.clone(),
            Content::Blocks(blocks) => yapi_types::message::blocks_text(blocks, "|"),
        },
        _ => String::new(),
    }
}
