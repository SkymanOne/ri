//! Refreshable model catalogs: pi.dev's overlay on built-in providers and
//! Radius gateway catalogs, against a mock server.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers; a panic is a test failure"
)]

use std::net::SocketAddr;
use std::path::PathBuf;

use ri_ai::model_catalog::{ModelsStore, RefreshOptions, Source, Target, refresh};
use ri_ai::registry::ModelRegistry;
use ri_mock::{Cassette, Interaction, MockServer, REDACTED, RequestMatch, Response};
use serde_json::{Value, json};

fn store(name: &str) -> (PathBuf, ModelsStore) {
    let dir = std::env::temp_dir().join(format!("ri-catalogs-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let store = ModelsStore::new(dir.join("models-store.json"));
    (dir, store)
}

fn get(path: &str, status: u16, headers: &[(&str, &str)], body: &Value) -> Interaction {
    Interaction {
        request: RequestMatch {
            method: "GET".into(),
            path: path.into(),
        },
        response: Response {
            status,
            headers: headers
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .chain([("content-type".to_owned(), "application/json".to_owned())])
                .collect(),
            chunks: if status == 304 {
                Vec::new()
            } else {
                vec![body.to_string()]
            },
            body_base64: None,
            chunk_delay_ms: 0,
        },
    }
}

async fn mock(interactions: Vec<Interaction>) -> MockServer {
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    MockServer::start(addr, Cassette { interactions })
        .await
        .unwrap()
}

fn chat_model(id: &str) -> Value {
    json!({
        "id": id, "name": id, "api": "openai-completions", "provider": "xai",
        "baseUrl": "https://api.x.ai/v1", "reasoning": false, "input": ["text"],
        "cost": {"input": 1, "output": 2, "cacheRead": 0, "cacheWrite": 0},
        "contextWindow": 128_000, "maxTokens": 8192
    })
}

fn ids(models: &[ri_types::model::Model]) -> Vec<&str> {
    models.iter().map(|model| model.id.as_str()).collect()
}

#[tokio::test]
async fn pi_dev_overlay_is_stored_revalidated_and_merged() {
    let server = mock(vec![
        get(
            "/api/models/providers/xai",
            200,
            &[
                ("last-modified", "Fri, 01 Jan 2100 00:00:00 GMT"),
                ("etag", "\"v1\""),
            ],
            &json!({"models": [chat_model("grok-next"), {"id": "grok-image", "type": "image"}, {"id": "clip", "type": "video"}]}),
        ),
        get("/api/models/providers/xai", 304, &[], &Value::Null),
    ])
    .await;
    let (dir, store) = store("overlay");
    let targets = [
        Target {
            provider: "xai".into(),
            source: Source::Remote,
            token: None,
            configured: true,
        },
        // Unconfigured providers are not fetched.
        Target {
            provider: "openai".into(),
            source: Source::Remote,
            token: None,
            configured: false,
        },
    ];
    let options = RefreshOptions {
        catalog_base_url: server.url(),
        ..RefreshOptions::default()
    };
    let refreshed = refresh(&targets, &store, &options).await;
    assert!(refreshed.errors.is_empty(), "{:?}", refreshed.errors);
    assert!(!refreshed.aborted);
    assert_eq!(ids(&refreshed.models["xai"].chat), ["grok-next"]);
    assert!(!refreshed.models.contains_key("openai"));
    let stored = store.read("xai").unwrap();
    assert_eq!(stored["etag"], "\"v1\"");
    assert_eq!(stored["lastModified"], 4_102_444_800_000_u64);
    // Image models are kept for later; unknown types are dropped.
    assert_eq!(stored["models"].as_array().unwrap().len(), 2);

    // A catalog checked within four hours is not fetched again.
    let fresh = refresh(&targets, &store, &options).await;
    assert_eq!(ids(&fresh.models["xai"].chat), ["grok-next"]);
    // A forced refresh revalidates; 304 keeps the stored catalog.
    let forced = RefreshOptions {
        force: true,
        ..options.clone()
    };
    let revalidated = refresh(&targets, &store, &forced).await;
    assert_eq!(ids(&revalidated.models["xai"].chat), ["grok-next"]);

    let requests = server.finish().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0].query.as_deref(),
        Some("types=chat%2Cimage%2Cclassifier")
    );
    assert_eq!(requests[1].headers["if-none-match"], "\"v1\"");

    // The overlay adds to the built-in catalog.
    let mut registry = ModelRegistry::builtin();
    let builtin = registry
        .models()
        .iter()
        .filter(|model| model.provider == "xai")
        .count();
    registry.apply_catalogs(revalidated.models);
    let merged: Vec<&str> = registry
        .models()
        .iter()
        .filter(|model| model.provider == "xai")
        .map(|model| model.id.as_str())
        .collect();
    assert_eq!(merged.len(), builtin + 1);
    assert!(merged.contains(&"grok-next"));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn an_overlay_older_than_the_build_is_ignored() {
    let server = mock(vec![get(
        "/api/models/providers/xai",
        200,
        &[("last-modified", "Thu, 01 Jan 2015 00:00:00 GMT")],
        &json!([chat_model("grok-old")]),
    )])
    .await;
    let (dir, store) = store("older");
    let targets = [Target {
        provider: "xai".into(),
        source: Source::Remote,
        token: None,
        configured: true,
    }];
    let options = RefreshOptions {
        catalog_base_url: server.url(),
        ..RefreshOptions::default()
    };
    let refreshed = refresh(&targets, &store, &options).await;
    assert!(refreshed.models["xai"].is_empty());
    assert_eq!(store.read("xai").unwrap()["models"][0]["id"], "grok-old");
    server.finish().unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn radius_gateway_catalog_is_fetched_with_the_token_and_restored_offline() {
    let server = mock(vec![
        get(
            "/v1/config",
            200,
            &[],
            &json!({
                "baseUrl": "https://radius.example/v1",
                "models": [
                    {
                        "id": "balanced", "name": "Balanced", "reasoning": true, "input": ["text", "image"],
                        "cost": {"input": 1, "output": 5, "cacheRead": 0.1, "cacheWrite": 1.25},
                        "contextWindow": 200_000, "maxTokens": 32_000
                    },
                    {"id": "incomplete", "name": "No cost"}
                ]
            }),
        ),
        get(
            "/v1/config",
            403,
            &[],
            &json!({"error": "forbidden"}),
        ),
    ])
    .await;
    let (dir, store) = store("radius");
    let target = Target {
        provider: "radius".into(),
        source: Source::Radius {
            gateway: server.url(),
        },
        token: Some("rad-access".into()),
        configured: true,
    };
    let options = RefreshOptions::default();
    let refreshed = refresh(std::slice::from_ref(&target), &store, &options).await;
    assert!(refreshed.errors.is_empty(), "{:?}", refreshed.errors);
    let models = &refreshed.models["radius"].chat;
    assert_eq!(ids(models), ["balanced"]);
    assert_eq!(models[0].api, "pi-messages");
    assert_eq!(models[0].base_url, "https://radius.example/v1");

    // Offline, the stored catalog comes back without a request.
    let offline = RefreshOptions {
        allow_network: false,
        ..RefreshOptions::default()
    };
    let restored = refresh(std::slice::from_ref(&target), &store, &offline).await;
    assert_eq!(ids(&restored.models["radius"].chat), ["balanced"]);

    // A failed fetch reports the gateway's answer and keeps the stored models.
    let failed = refresh(std::slice::from_ref(&target), &store, &options).await;
    assert_eq!(ids(&failed.models["radius"].chat), ["balanced"]);
    assert_eq!(
        failed.errors,
        [(
            "radius".to_owned(),
            format!(
                "Could not load Radius config from {}: 403: {{\"error\":\"forbidden\"}}",
                server.url()
            )
        )]
    );
    let requests = server.finish().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].headers["authorization"], REDACTED);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn llama_server_models_become_chat_models_and_classifiers() {
    let server = mock(vec![
        get(
            "/models",
            200,
            &[],
            &json!({"data": [
                {"id": "qwen", "status": {"value": "loaded", "args": ["--ctx-size", "16384"]},
                 "architecture": {"input_modalities": ["text", "image"]}},
                {"id": "preset", "status": {"value": "unloaded"}, "source": "preset"},
                {"id": "cold", "status": {"value": "unloaded"}}
            ]}),
        ),
        // The router does not autoload presets, so only `qwen` is served.
        get("/props", 200, &[], &json!({"models_autoload": false})),
        get(
            "/props",
            200,
            &[],
            &json!({"chat_template": "{%- if enable_thinking %}<think>{% endif %}"}),
        ),
    ])
    .await;
    let (dir, store) = store("llama");
    let target = Target {
        provider: "llama.cpp".into(),
        source: Source::Llama {
            server: format!("{}/v1/", server.url()),
        },
        token: Some("secret".into()),
        configured: true,
    };
    let refreshed = refresh(
        std::slice::from_ref(&target),
        &store,
        &RefreshOptions::default(),
    )
    .await;
    assert!(refreshed.errors.is_empty(), "{:?}", refreshed.errors);
    let models = &refreshed.models["llama.cpp"];
    assert_eq!(ids(&models.chat), ["qwen"]);
    let chat = &models.chat[0];
    assert!(chat.reasoning);
    assert!(chat.accepts_images());
    assert_eq!(chat.context_window, 16_384);
    assert_eq!(chat.base_url, format!("{}/v1", server.url()));
    assert_eq!(models.classifiers.len(), 1);
    assert_eq!(models.classifiers[0].api, "llama-cpp-classify");
    assert_eq!(models.classifiers[0].base_url, server.url());

    // Restored offline, as pi publishes the stored catalog.
    let offline = RefreshOptions {
        allow_network: false,
        ..RefreshOptions::default()
    };
    let restored = refresh(std::slice::from_ref(&target), &store, &offline).await;
    assert_eq!(restored.models["llama.cpp"], *models);

    let requests = server.finish().unwrap();
    assert_eq!(requests[0].headers["authorization"], REDACTED);
    assert_eq!(
        requests[2].query.as_deref(),
        Some("model=qwen&autoload=false")
    );
    std::fs::remove_dir_all(&dir).unwrap();
}
