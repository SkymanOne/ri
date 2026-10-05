//! Provider-specific credentials and endpoints through the registry, against
//! a mock server.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers; a panic is a test failure"
)]

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use yapi_ai::api::Apis;
use yapi_ai::catalog::builtin_models;
use yapi_ai::credentials::ProviderEnv;
use yapi_ai::registry::ModelRegistry;
use yapi_ai::stream::{Request, StreamOptions};
use yapi_mock::{Cassette, Interaction, MockServer, REDACTED, RequestMatch, Response};
use yapi_types::message::{Content, Message, StopReason, UserMessage};

fn agent_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("yapi-providers-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn cassette(path: &str, chunks_from: &str) -> Cassette {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/cassettes")
        .join(chunks_from);
    let mut cassette = Cassette::load(&source).unwrap();
    cassette.interactions[0].request.path = path.to_owned();
    cassette
}

async fn mock(cassette: Cassette) -> MockServer {
    MockServer::local(cassette).await.unwrap()
}

fn user(text: &str) -> Message {
    Message::User(UserMessage {
        content: Content::Text(text.into()),
        timestamp: 1,
    })
}

async fn ask(registry: &ModelRegistry, provider: &str, model: &str) -> StopReason {
    let model = registry.find(provider, model).unwrap().clone();
    let mut request = Request {
        model,
        messages: vec![user("hi")],
        options: StreamOptions::default(),
    };
    registry
        .auth(&request.model)
        .await
        .apply(&mut request)
        .unwrap();
    let message = Apis::default().stream(request).result().await.unwrap();
    assert_eq!(message.error_message, None);
    message.stop_reason
}

#[tokio::test]
async fn cloudflare_workers_ai_fills_the_account_from_the_credential() {
    let server = mock(cassette(
        "/client/v4/accounts/acct-1/ai/v1/chat/completions",
        "openai-completions/text.json",
    ))
    .await;
    let dir = agent_dir("workers");
    std::fs::write(
        dir.join("auth.json"),
        r#"{"cloudflare-workers-ai": {"type": "api_key", "key": "cf-key", "env": {"CLOUDFLARE_ACCOUNT_ID": "acct-1"}}}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("models.json"),
        format!(
            r#"{{"providers": {{"cloudflare-workers-ai": {{"baseUrl": "{}/client/v4/accounts/{{CLOUDFLARE_ACCOUNT_ID}}/ai/v1"}}}}}}"#,
            server.url()
        ),
    )
    .unwrap();
    let registry = ModelRegistry::load(&dir);
    let model = registry
        .models()
        .iter()
        .find(|model| model.provider == "cloudflare-workers-ai")
        .unwrap()
        .id
        .clone();
    assert!(registry.has_auth("cloudflare-workers-ai"));
    assert_eq!(
        ask(&registry, "cloudflare-workers-ai", &model).await,
        StopReason::Stop
    );
    let requests = server.finish().unwrap();
    assert_eq!(requests[0].headers["authorization"], REDACTED);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn cloudflare_gateway_sends_its_key_in_the_gateway_header() {
    let server = mock(cassette(
        "/v1/acct-2/gw-2/compat/chat/completions",
        "openai-completions/text.json",
    ))
    .await;
    let dir = agent_dir("gateway");
    std::fs::write(
        dir.join("auth.json"),
        r#"{"cloudflare-ai-gateway": {"type": "api_key", "key": "cf-key", "env": {"CLOUDFLARE_ACCOUNT_ID": "acct-2", "CLOUDFLARE_GATEWAY_ID": "gw-2"}}}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("models.json"),
        format!(
            r#"{{"providers": {{"cloudflare-ai-gateway": {{"baseUrl": "{}/v1/{{CLOUDFLARE_ACCOUNT_ID}}/{{CLOUDFLARE_GATEWAY_ID}}/compat"}}}}}}"#,
            server.url()
        ),
    )
    .unwrap();
    let registry = ModelRegistry::load(&dir);
    let model = registry
        .models()
        .iter()
        .find(|model| {
            model.provider == "cloudflare-ai-gateway" && model.api == "openai-completions"
        })
        .unwrap()
        .id
        .clone();
    let auth = registry
        .auth(registry.find("cloudflare-ai-gateway", &model).unwrap())
        .await;
    assert_eq!(auth.api_key, None);
    assert_eq!(auth.source.as_deref(), Some("stored credential"));
    assert_eq!(
        ask(&registry, "cloudflare-ai-gateway", &model).await,
        StopReason::Stop
    );
    let requests = server.finish().unwrap();
    assert_eq!(requests[0].headers["cf-aig-authorization"], REDACTED);
    assert!(!requests[0].headers.contains_key("authorization"));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn anthropic_federation_exchanges_the_identity_token_and_retries_a_401() {
    let text = cassette("/v1/messages", "anthropic-messages/text.json");
    let token = |access: &str| {
        Interaction::json(
            "POST",
            "/v1/oauth/token",
            200,
            &json!({"access_token": access, "token_type": "Bearer", "expires_in": 3600}),
        )
    };
    let mut interactions = vec![
        token("fed-1"),
        Interaction::json(
            "POST",
            "/v1/messages",
            401,
            &json!({"type": "error", "error": {"type": "authentication_error", "message": "expired"}}),
        ),
        token("fed-2"),
    ];
    interactions.extend(text.interactions);
    let server = mock(Cassette { interactions }).await;
    let dir = agent_dir("federation");
    let token_file = dir.join("identity.jwt");
    std::fs::write(&token_file, "header.payload.sig\n").unwrap();
    let mut env = ProviderEnv::new();
    for (name, value) in [
        ("ANTHROPIC_FEDERATION_RULE_ID", "fdrl_1"),
        ("ANTHROPIC_ORGANIZATION_ID", "org_1"),
        (
            "ANTHROPIC_IDENTITY_TOKEN_FILE",
            token_file.to_str().unwrap(),
        ),
        ("ANTHROPIC_WORKSPACE_ID", "wrkspc_1"),
    ] {
        env.insert(name.into(), value.into());
    }
    let mut model = builtin_models("anthropic")
        .into_iter()
        .find(|model| model.id == "claude-sonnet-4-5")
        .unwrap();
    model.base_url = server.url();
    let request = Request {
        model,
        messages: vec![user("hi")],
        options: StreamOptions {
            env: Some(env),
            ..StreamOptions::default()
        },
    };
    let message = Apis::default().stream(request).result().await.unwrap();
    assert_eq!(message.error_message, None);
    assert_eq!(message.stop_reason, StopReason::Stop);
    let requests = server.finish().unwrap();
    let exchange: Value = serde_json::from_str(&requests[0].body).unwrap();
    assert_eq!(
        exchange,
        json!({
            "grant_type": "urn:ietf:params:oauth:grant-type:jwt-bearer",
            "assertion": "header.payload.sig",
            "federation_rule_id": "fdrl_1",
            "organization_id": "org_1",
            "workspace_id": "wrkspc_1",
        })
    );
    assert_eq!(
        requests[0].headers["anthropic-beta"],
        "oauth-2025-04-20,oidc-federation-2026-04-01"
    );
    let retried = &requests[3];
    assert_eq!(retried.headers["authorization"], REDACTED);
    assert!(!retried.headers.contains_key("x-api-key"));
    assert_eq!(retried.headers["anthropic-beta"], "oauth-2025-04-20");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A fresh RSA key as a PKCS#8 PEM.
fn rsa_pem() -> String {
    use aws_lc_rs::encoding::{AsDer, Pkcs8V1Der};
    use aws_lc_rs::rsa::{KeyPair, KeySize};
    use base64::Engine as _;
    let key = KeyPair::generate(KeySize::Rsa2048).unwrap();
    let der: Pkcs8V1Der<'static> = key.as_der().unwrap();
    let body = base64::engine::general_purpose::STANDARD.encode(der.as_ref());
    format!("-----BEGIN PRIVATE KEY-----\n{body}\n-----END PRIVATE KEY-----\n")
}

#[tokio::test]
async fn vertex_signs_in_with_a_service_account() {
    let mut interactions = vec![Interaction::json(
        "POST",
        "/token",
        200,
        &json!({"access_token": "ya29.sa", "expires_in": 3599, "token_type": "Bearer"}),
    )];
    interactions.extend(
        cassette(
            "/v1/publishers/google/models/gemini-2.5-flash:streamGenerateContent",
            "google-generative-ai/text.json",
        )
        .interactions,
    );
    let server = mock(Cassette { interactions }).await;
    let dir = agent_dir("vertex");
    let key_file = dir.join("sa.json");
    std::fs::write(
        &key_file,
        json!({
            "type": "service_account",
            "client_email": "sa@proj-1.iam.gserviceaccount.com",
            "private_key": rsa_pem(),
            "token_uri": format!("{}/token", server.url()),
            "quota_project_id": "billing-1",
        })
        .to_string(),
    )
    .unwrap();
    let mut env = ProviderEnv::new();
    for (name, value) in [
        ("GOOGLE_CLOUD_PROJECT", "proj-1"),
        ("GOOGLE_CLOUD_LOCATION", "us-central1"),
        ("GOOGLE_APPLICATION_CREDENTIALS", key_file.to_str().unwrap()),
    ] {
        env.insert(name.into(), value.into());
    }
    let mut model = builtin_models("google-vertex")
        .into_iter()
        .find(|model| model.id == "gemini-2.5-flash")
        .unwrap();
    model.base_url = server.url();
    let request = Request {
        model,
        messages: vec![user("hi")],
        options: StreamOptions {
            env: Some(env),
            ..StreamOptions::default()
        },
    };
    let message = Apis::default().stream(request).result().await.unwrap();
    assert_eq!(message.error_message, None);
    assert_eq!(message.api, "google-vertex");
    let requests = server.finish().unwrap();
    let form: Vec<(String, String)> = url::form_urlencoded::parse(requests[0].body.as_bytes())
        .into_owned()
        .collect();
    assert_eq!(
        form[0],
        (
            "grant_type".to_owned(),
            "urn:ietf:params:oauth:grant-type:jwt-bearer".to_owned()
        )
    );
    assert_eq!(form[1].0, "assertion");
    assert_eq!(requests[1].headers["authorization"], REDACTED);
    assert_eq!(requests[1].headers["x-goog-user-project"], "billing-1");
    assert!(!requests[1].headers.contains_key("x-goog-api-key"));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn google_adc_refreshes_users_and_impersonates() {
    let server = mock(Cassette {
        interactions: vec![
            Interaction::json(
                "POST",
                "/token",
                200,
                &json!({"access_token": "ya29.user", "expires_in": 3599}),
            ),
            Interaction::json(
                "POST",
                "/v1/projects/-/serviceAccounts/sa@p.iam.gserviceaccount.com:generateAccessToken",
                200,
                &json!({"accessToken": "ya29.sa", "expireTime": "2099-01-01T00:00:00Z"}),
            ),
            Interaction::json("POST", "/token", 400, &json!({"error": "invalid_grant"})),
        ],
    })
    .await;
    let dir = agent_dir("adc");
    let file = dir.join("adc.json");
    std::fs::write(
        &file,
        json!({
            "type": "impersonated_service_account",
            "service_account_impersonation_url": format!(
                "{}/v1/projects/-/serviceAccounts/sa@p.iam.gserviceaccount.com:generateAccessToken",
                server.url()
            ),
            "delegates": [],
            "source_credentials": {
                "type": "authorized_user",
                "client_id": "cid",
                "client_secret": "secret",
                "refresh_token": "1//rt",
            },
        })
        .to_string(),
    )
    .unwrap();
    let token_url = format!("{}/token", server.url());
    let cancel = tokio_util::sync::CancellationToken::new();
    let token = yapi_ai::auth::google_adc::token(
        Some(file.to_string_lossy().into_owned()),
        &token_url,
        &cancel,
    )
    .await
    .unwrap();
    assert_eq!(token.access_token, "ya29.sa");
    // The cached token serves the next request.
    let again = yapi_ai::auth::google_adc::token(
        Some(file.to_string_lossy().into_owned()),
        &token_url,
        &cancel,
    )
    .await
    .unwrap();
    assert_eq!(again, token);
    let user = dir.join("user.json");
    std::fs::write(
        &user,
        json!({"type": "authorized_user", "client_id": "c", "client_secret": "s", "refresh_token": "r"})
            .to_string(),
    )
    .unwrap();
    let failed = yapi_ai::auth::google_adc::token(
        Some(user.to_string_lossy().into_owned()),
        &token_url,
        &cancel,
    )
    .await;
    assert_eq!(failed, Err("invalid_grant".to_owned()));
    let requests = server.finish().unwrap();
    assert_eq!(
        requests[0].body,
        "refresh_token=1%2F%2Frt&client_id=cid&client_secret=secret&grant_type=refresh_token"
    );
    let impersonation: Value = serde_json::from_str(&requests[1].body).unwrap();
    assert_eq!(
        impersonation,
        json!({"delegates": [], "scope": ["https://www.googleapis.com/auth/cloud-platform"], "lifetime": "3600s"})
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The transcript pi's request goldens below were captured with.
fn bedrock_transcript(model_id: &str) -> Vec<Message> {
    let value = json!([
        {"role": "system", "content": "Be brief.", "toolsAdded": [{"name": "read", "description": "Read a file", "parameters": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}}], "timestamp": 1},
        {"role": "user", "content": [{"type": "text", "text": "look"}, {"type": "image", "data": "iVBORw0KGgo=", "mimeType": "image/png"}], "timestamp": 2},
        {"role": "assistant", "content": [{"type": "thinking", "thinking": "I should read", "thinkingSignature": "sig1"}, {"type": "text", "text": "Reading."}, {"type": "toolCall", "id": "toolu_1", "name": "read", "arguments": {"path": "a.txt"}}], "api": "bedrock-converse-stream", "provider": "amazon-bedrock", "model": model_id, "usage": {"input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 2, "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}}, "stopReason": "toolUse", "timestamp": 3},
        {"role": "toolResult", "toolCallId": "toolu_1", "toolName": "read", "content": [{"type": "text", "text": "hello"}], "isError": false, "timestamp": 4},
        {"role": "user", "content": "thanks", "timestamp": 5}
    ]);
    serde_json::from_value(value).unwrap()
}

fn event(kind: &str, payload: &Value) -> Vec<u8> {
    yapi_ai::aws::eventstream::encode(
        &[
            (":event-type", kind),
            (":content-type", "application/json"),
            (":message-type", "event"),
        ],
        payload.to_string().as_bytes(),
    )
}

fn exception(kind: &str, payload: &Value) -> Vec<u8> {
    yapi_ai::aws::eventstream::encode(
        &[
            (":exception-type", kind),
            (":content-type", "application/json"),
            (":message-type", "exception"),
        ],
        payload.to_string().as_bytes(),
    )
}

fn binary_exchange(path: &str, status: u16, headers: &[(&str, &str)], body: &[u8]) -> Interaction {
    use base64::Engine as _;
    Interaction {
        request: RequestMatch {
            method: "POST".into(),
            path: path.into(),
        },
        response: Response {
            status,
            headers: headers
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            chunks: Vec::new(),
            body_base64: Some(base64::engine::general_purpose::STANDARD.encode(body)),
            chunk_delay_ms: 0,
        },
    }
}

fn simple_stream() -> Vec<u8> {
    [
        event("messageStart", &json!({"role": "assistant"})),
        event("contentBlockDelta", &json!({"contentBlockIndex": 0, "delta": {"text": "Hi"}})),
        event("contentBlockStop", &json!({"contentBlockIndex": 0})),
        event("messageStop", &json!({"stopReason": "end_turn"})),
        event(
            "metadata",
            &json!({"usage": {"inputTokens": 3, "outputTokens": 1, "totalTokens": 4}, "metrics": {"latencyMs": 1}}),
        ),
    ]
    .concat()
}

fn bedrock_env(vars: &[(&str, &str)]) -> ProviderEnv {
    let mut env: ProviderEnv = [
        ("AWS_ACCESS_KEY_ID", "AKIDEXAMPLE"),
        ("AWS_SECRET_ACCESS_KEY", "fake-secret"),
        ("AWS_REGION", "us-east-1"),
        ("AWS_MAX_ATTEMPTS", "1"),
    ]
    .iter()
    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
    .collect();
    for (name, value) in vars {
        env.insert((*name).to_owned(), (*value).to_owned());
    }
    env
}

async fn bedrock(
    server: &MockServer,
    model_id: &str,
    messages: Vec<Message>,
    options: StreamOptions,
) -> (
    Vec<yapi_ai::stream::StreamEvent>,
    yapi_types::message::AssistantMessage,
) {
    let mut model = builtin_models("amazon-bedrock")
        .into_iter()
        .find(|model| model.id == model_id)
        .unwrap();
    model.base_url = server.url();
    model.input = vec![
        yapi_types::models::InputKind::Text,
        yapi_types::models::InputKind::Image,
    ];
    let mut stream = Apis::default().stream(Request {
        model,
        messages,
        options,
    });
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    let last = match events.last().unwrap() {
        yapi_ai::stream::StreamEvent::Done(message)
        | yapi_ai::stream::StreamEvent::Error(message) => message.clone(),
        other => panic!("stream ended with {other:?}"),
    };
    (events, last)
}

#[tokio::test]
async fn bedrock_sends_pi_request_bodies() {
    // Bodies pi's Bedrock provider sent for the same transcript.
    let cases = [
        (
            "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
            "/model/us.anthropic.claude-sonnet-4-5-20250929-v1%3A0/converse-stream",
            yapi_types::message::ThinkingLevel::High,
            r#"{"messages":[{"role":"user","content":[{"text":"look"},{"image":{"format":"png","source":{"bytes":"iVBORw0KGgo="}}}]},{"role":"assistant","content":[{"reasoningContent":{"reasoningText":{"text":"I should read","signature":"sig1"}}},{"text":"Reading."},{"toolUse":{"toolUseId":"toolu_1","name":"read","input":{"path":"a.txt"}}}]},{"role":"user","content":[{"toolResult":{"toolUseId":"toolu_1","content":[{"text":"hello"}],"status":"success"}}]},{"role":"user","content":[{"text":"thanks"},{"cachePoint":{"type":"default"}}]}],"system":[{"text":"Be brief."},{"cachePoint":{"type":"default"}}],"inferenceConfig":{"maxTokens":20384},"toolConfig":{"tools":[{"toolSpec":{"name":"read","inputSchema":{"json":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}},"description":"Read a file"}}]},"additionalModelRequestFields":{"thinking":{"type":"enabled","budget_tokens":16384,"display":"summarized"},"anthropic_beta":["interleaved-thinking-2025-05-14"]}}"#,
        ),
        (
            "global.anthropic.claude-opus-4-6-v1",
            "/model/global.anthropic.claude-opus-4-6-v1/converse-stream",
            yapi_types::message::ThinkingLevel::Xhigh,
            r#"{"messages":[{"role":"user","content":[{"text":"look"},{"image":{"format":"png","source":{"bytes":"iVBORw0KGgo="}}}]},{"role":"assistant","content":[{"reasoningContent":{"reasoningText":{"text":"I should read","signature":"sig1"}}},{"text":"Reading."},{"toolUse":{"toolUseId":"toolu_1","name":"read","input":{"path":"a.txt"}}}]},{"role":"user","content":[{"toolResult":{"toolUseId":"toolu_1","content":[{"text":"hello"}],"status":"success"}}]},{"role":"user","content":[{"text":"thanks"},{"cachePoint":{"type":"default"}}]}],"system":[{"text":"Be brief."},{"cachePoint":{"type":"default"}}],"inferenceConfig":{"maxTokens":4000},"toolConfig":{"tools":[{"toolSpec":{"name":"read","inputSchema":{"json":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}},"description":"Read a file"}}]},"additionalModelRequestFields":{"thinking":{"type":"adaptive","display":"summarized"},"output_config":{"effort":"high"}}}"#,
        ),
        (
            "openai.gpt-oss-120b-1:0",
            "/model/openai.gpt-oss-120b-1%3A0/converse-stream",
            yapi_types::message::ThinkingLevel::Medium,
            r#"{"messages":[{"role":"user","content":[{"text":"look"},{"image":{"format":"png","source":{"bytes":"iVBORw0KGgo="}}}]},{"role":"assistant","content":[{"reasoningContent":{"reasoningText":{"text":"I should read"}}},{"text":"Reading."},{"toolUse":{"toolUseId":"toolu_1","name":"read","input":{"path":"a.txt"}}}]},{"role":"user","content":[{"toolResult":{"toolUseId":"toolu_1","content":[{"text":"hello"}],"status":"success"}}]},{"role":"user","content":[{"text":"thanks"}]}],"system":[{"text":"Be brief."}],"inferenceConfig":{"maxTokens":4000},"toolConfig":{"tools":[{"toolSpec":{"name":"read","inputSchema":{"json":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}},"description":"Read a file"}}]}}"#,
        ),
        (
            "us.anthropic.claude-opus-4-8",
            "/model/us.anthropic.claude-opus-4-8/converse-stream",
            yapi_types::message::ThinkingLevel::Xhigh,
            r#"{"messages":[{"role":"user","content":[{"text":"look"},{"image":{"format":"png","source":{"bytes":"iVBORw0KGgo="}}}]},{"role":"assistant","content":[{"reasoningContent":{"reasoningText":{"text":"I should read","signature":"sig1"}}},{"text":"Reading."},{"toolUse":{"toolUseId":"toolu_1","name":"read","input":{"path":"a.txt"}}}]},{"role":"user","content":[{"toolResult":{"toolUseId":"toolu_1","content":[{"text":"hello"}],"status":"success"}}]},{"role":"user","content":[{"text":"thanks"},{"cachePoint":{"type":"default"}}]}],"system":[{"text":"Be brief."},{"cachePoint":{"type":"default"}}],"inferenceConfig":{"maxTokens":4000},"toolConfig":{"tools":[{"toolSpec":{"name":"read","inputSchema":{"json":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}},"description":"Read a file"}}]},"additionalModelRequestFields":{"thinking":{"type":"adaptive","display":"summarized"},"output_config":{"effort":"xhigh"}}}"#,
        ),
    ];
    for (model_id, path, level, expected) in cases {
        let server = mock(Cassette {
            interactions: vec![binary_exchange(
                path,
                200,
                &[("content-type", "application/vnd.amazon.eventstream")],
                &simple_stream(),
            )],
        })
        .await;
        let (events, message) = bedrock(
            &server,
            model_id,
            bedrock_transcript(model_id),
            StreamOptions {
                env: Some(bedrock_env(&[])),
                reasoning: Some(level),
                max_tokens: Some(4000),
                ..StreamOptions::default()
            },
        )
        .await;
        assert_eq!(message.error_message, None, "{model_id}");
        assert_eq!(message.stop_reason, StopReason::Stop);
        assert!(matches!(events[0], yapi_ai::stream::StreamEvent::Start(_)));
        let requests = server.finish().unwrap();
        assert_eq!(requests[0].body, expected, "{model_id}");
        assert_eq!(requests[0].headers["authorization"], REDACTED);
        assert!(requests[0].headers.contains_key("x-amz-date"));
        assert_eq!(
            requests[0].headers["x-amz-content-sha256"],
            yapi_ai::aws::sigv4::sha256_hex(expected.as_bytes())
        );
    }
}

#[tokio::test]
async fn bedrock_streams_tools_reasoning_and_usage() {
    let stream = [
        event("messageStart", &json!({"role": "assistant"})),
        event("contentBlockDelta", &json!({"contentBlockIndex": 0, "delta": {"reasoningContent": {"text": "think"}}})),
        event("contentBlockDelta", &json!({"contentBlockIndex": 0, "delta": {"reasoningContent": {"signature": "sig"}}})),
        event("contentBlockStop", &json!({"contentBlockIndex": 0})),
        event("contentBlockStart", &json!({"contentBlockIndex": 1, "start": {"toolUse": {"toolUseId": "tool-1", "name": "read"}}})),
        event("contentBlockDelta", &json!({"contentBlockIndex": 1, "delta": {"toolUse": {"input": "{\"path\":"}}})),
        event("contentBlockDelta", &json!({"contentBlockIndex": 1, "delta": {"toolUse": {"input": "\"a.txt\"}"}}})),
        event("contentBlockStop", &json!({"contentBlockIndex": 1})),
        event("messageStop", &json!({"stopReason": "tool_use"})),
        event("metadata", &json!({"usage": {"inputTokens": 10, "outputTokens": 5, "totalTokens": 30, "cacheReadInputTokens": 15, "cacheDetails": [{"ttl": "1h", "inputTokens": 7}]}})),
    ]
    .concat();
    let server = mock(Cassette {
        interactions: vec![binary_exchange(
            "/model/us.anthropic.claude-sonnet-4-5-20250929-v1%3A0/converse-stream",
            200,
            &[("content-type", "application/vnd.amazon.eventstream")],
            &stream,
        )],
    })
    .await;
    let (events, message) = bedrock(
        &server,
        "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
        vec![user("hi")],
        StreamOptions {
            api_key: Some("bedrock-bearer".into()),
            env: Some(bedrock_env(&[])),
            ..StreamOptions::default()
        },
    )
    .await;
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    let value = serde_json::to_value(&message).unwrap();
    assert_eq!(
        value["content"],
        json!([
            {"type": "thinking", "thinking": "think", "thinkingSignature": "sig"},
            {"type": "toolCall", "id": "tool-1", "name": "read", "arguments": {"path": "a.txt"}}
        ])
    );
    assert_eq!(value["usage"]["cacheRead"], 15);
    assert_eq!(value["usage"]["cacheWrite1h"], 7);
    assert_eq!(value["usage"]["totalTokens"], 30);
    let kinds: Vec<&str> = events
        .iter()
        .map(|event| match event {
            yapi_ai::stream::StreamEvent::Start(_) => "start",
            yapi_ai::stream::StreamEvent::Update { event, .. } => match event {
                yapi_types::event::AssistantMessageEvent::ThinkingStart { .. } => "thinking_start",
                yapi_types::event::AssistantMessageEvent::ThinkingDelta { .. } => "thinking_delta",
                yapi_types::event::AssistantMessageEvent::ThinkingEnd { .. } => "thinking_end",
                yapi_types::event::AssistantMessageEvent::ToolcallStart { .. } => "toolcall_start",
                yapi_types::event::AssistantMessageEvent::ToolcallDelta { .. } => "toolcall_delta",
                yapi_types::event::AssistantMessageEvent::ToolcallEnd { .. } => "toolcall_end",
                _ => "other",
            },
            yapi_ai::stream::StreamEvent::Done(_) => "done",
            yapi_ai::stream::StreamEvent::Error(_) => "error",
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "start",
            "thinking_start",
            "thinking_delta",
            "thinking_end",
            "toolcall_start",
            "toolcall_delta",
            "toolcall_delta",
            "toolcall_end",
            "done"
        ]
    );
    let requests = server.finish().unwrap();
    // A bearer token replaces SigV4.
    assert_eq!(requests[0].headers["authorization"], REDACTED);
    assert!(!requests[0].headers.contains_key("x-amz-date"));
}

#[tokio::test]
async fn bedrock_reports_errors_like_pi() {
    let path = "/model/us.anthropic.claude-sonnet-4-5-20250929-v1%3A0/converse-stream";
    let cases: Vec<(Interaction, &str, Value)> = vec![
        (
            binary_exchange(
                path,
                400,
                &[
                    ("content-type", "application/json"),
                    (
                        "x-amzn-errortype",
                        "ValidationException:http://internal.amazon.com/coral/com.amazon.bedrock/",
                    ),
                    ("x-amzn-requestid", "rid-400"),
                ],
                br#"{"message":"The provided model identifier is invalid."}"#,
            ),
            "Validation error: The provided model identifier is invalid.",
            json!({"status": 400, "errorCode": "ValidationException", "requestId": "rid-400"}),
        ),
        (
            binary_exchange(
                path,
                200,
                &[
                    ("content-type", "application/vnd.amazon.eventstream"),
                    ("x-amzn-requestid", "req-1"),
                ],
                &[
                    event("messageStart", &json!({"role": "assistant"})),
                    event(
                        "contentBlockDelta",
                        &json!({"contentBlockIndex": 0, "delta": {"text": "Hi"}}),
                    ),
                    exception(
                        "throttlingException",
                        &json!({"message": "Too many tokens, please wait before trying again."}),
                    ),
                ]
                .concat(),
            ),
            "Throttling error: Too many tokens, please wait before trying again.",
            json!({"errorCode": "ThrottlingException", "requestId": "req-1"}),
        ),
        (
            binary_exchange(
                path,
                200,
                &[
                    ("content-type", "application/vnd.amazon.eventstream"),
                    ("x-amzn-requestid", "req-1"),
                ],
                &[
                    event("messageStart", &json!({"role": "assistant"})),
                    event(
                        "messageStop",
                        &json!({"stopReason": "guardrail_intervened"}),
                    ),
                ]
                .concat(),
            ),
            "Provider stopped with: guardrail_intervened",
            json!({"requestId": "req-1"}),
        ),
    ];
    for (interaction, message, details) in cases {
        let server = mock(Cassette {
            interactions: vec![interaction],
        })
        .await;
        let (_, result) = bedrock(
            &server,
            "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
            vec![user("hi")],
            StreamOptions {
                env: Some(bedrock_env(&[])),
                ..StreamOptions::default()
            },
        )
        .await;
        assert_eq!(result.stop_reason, StopReason::Error);
        assert_eq!(result.error_message.as_deref(), Some(message));
        let diagnostic = &result.diagnostics.unwrap()[0];
        assert_eq!(diagnostic["type"], "bedrock_response_failure");
        assert_eq!(diagnostic["details"], details);
        server.finish().unwrap();
    }
}

#[tokio::test]
async fn aws_chain_assumes_roles_and_reads_sso_and_container_credentials() {
    use yapi_ai::aws::{AwsEnv, default_credentials};
    let sts_xml = "<AssumeRoleResponse><AssumeRoleResult><Credentials><AccessKeyId>ASIAROLE</AccessKeyId><SecretAccessKey>role-secret</SecretAccessKey><SessionToken>role-token</SessionToken><Expiration>2099-01-01T00:00:00Z</Expiration></Credentials></AssumeRoleResult></AssumeRoleResponse>";
    let mut sts = Interaction::json("POST", "/", 200, &Value::Null);
    sts.response
        .headers
        .insert("content-type".into(), "text/xml".into());
    sts.response.chunks = vec![sts_xml.into()];
    let server = mock(Cassette {
        interactions: vec![
            sts,
            Interaction::json(
                "GET",
                "/federation/credentials",
                200,
                &json!({"roleCredentials": {"accessKeyId": "ASIASSO", "secretAccessKey": "s", "sessionToken": "t", "expiration": 4_070_908_800_000_u64}}),
            ),
            Interaction::json(
                "GET",
                "/creds",
                200,
                &json!({"AccessKeyId": "ASIAECS", "SecretAccessKey": "s", "Token": "t", "Expiration": "2099-01-01T00:00:00Z"}),
            ),
        ],
    })
    .await;
    let home = agent_dir("aws-chain");
    std::fs::create_dir_all(home.join(".aws/sso/cache")).unwrap();
    std::fs::write(
        home.join(".aws/credentials"),
        "[base]\naws_access_key_id = AKIDBASE\naws_secret_access_key = base-secret\n",
    )
    .unwrap();
    std::fs::write(
        home.join(".aws/config"),
        "[profile role]\nrole_arn = arn:aws:iam::123:role/dev\nsource_profile = base\nrole_session_name = yapi-test\n\n[profile sso]\nsso_session = corp\nsso_account_id = 123\nsso_role_name = Dev\n\n[sso-session corp]\nsso_region = us-east-1\nsso_start_url = https://corp.awsapps.com/start\n",
    )
    .unwrap();
    // SHA-1 of the session name `corp`.
    let cache_name = "ee0bfd2552fbd840c02cc48b6e823320543c450f";
    std::fs::write(
        home.join(".aws/sso/cache")
            .join(format!("{cache_name}.json")),
        json!({"accessToken": "sso-access", "expiresAt": "2099-01-01T00:00:00Z"}).to_string(),
    )
    .unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let url = server.url();
    let env = AwsEnv::from_vars([
        ("HOME", home.to_str().unwrap()),
        ("AWS_ENDPOINT_URL_STS", url.as_str()),
        ("AWS_ENDPOINT_URL_SSO", url.as_str()),
        ("AWS_EC2_METADATA_DISABLED", "true"),
    ]);
    let role = default_credentials(&env, Some("role"), Some("us-east-1"), &cancel)
        .await
        .unwrap();
    assert_eq!(role.access_key_id, "ASIAROLE");
    assert_eq!(role.session_token.as_deref(), Some("role-token"));
    let sso = default_credentials(&env, Some("sso"), None, &cancel)
        .await
        .unwrap();
    assert_eq!(sso.access_key_id, "ASIASSO");
    let full_uri = format!("{url}/creds");
    let container = AwsEnv::from_vars([
        ("HOME", home.to_str().unwrap()),
        ("AWS_CONTAINER_CREDENTIALS_FULL_URI", full_uri.as_str()),
        ("AWS_CONTAINER_AUTHORIZATION_TOKEN", "ecs-token"),
    ]);
    let ecs = default_credentials(&container, None, None, &cancel)
        .await
        .unwrap();
    assert_eq!(ecs.access_key_id, "ASIAECS");
    let requests = server.finish().unwrap();
    let form: Vec<(String, String)> = url::form_urlencoded::parse(requests[0].body.as_bytes())
        .into_owned()
        .collect();
    assert_eq!(form[0], ("Action".into(), "AssumeRole".into()));
    assert!(form.contains(&("RoleSessionName".into(), "yapi-test".into())));
    assert!(requests[0].headers.contains_key("x-amz-date"));
    assert_eq!(
        requests[1].query.as_deref(),
        Some("role_name=Dev&account_id=123")
    );
    assert_eq!(requests[2].headers["authorization"], REDACTED);
    std::fs::remove_dir_all(&home).unwrap();
}
