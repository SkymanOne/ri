//! OAuth sign-ins and refresh against a mock authorization server.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers; a panic is a test failure"
)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use ri_ai::auth::anthropic::AnthropicOAuth;
use ri_ai::auth::chatgpt::ChatGptOAuth;
use ri_ai::auth::codex::CodexOAuth;
use ri_ai::auth::copilot::CopilotOAuth;
use ri_ai::auth::{AuthEvent, AuthPrompt, AuthRequest, Interaction, LoginOptions, OAuthProvider};
use ri_ai::registry::{LoginKind, ModelRegistry};
use ri_mock::{Cassette, Interaction as Exchange, MockServer, RequestMatch, Response};
use ri_types::auth::{Credential, OAuthCredential};
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_util::sync::CancellationToken;

fn exchange(method: &str, path: &str, status: u16, body: &Value) -> Exchange {
    Exchange {
        request: RequestMatch {
            method: method.into(),
            path: path.into(),
        },
        response: Response {
            status,
            headers: [("content-type".to_owned(), "application/json".to_owned())]
                .into_iter()
                .collect(),
            chunks: vec![body.to_string()],
            body_base64: None,
            chunk_delay_ms: 0,
        },
    }
}

async fn mock(interactions: Vec<Exchange>) -> MockServer {
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    MockServer::start(addr, Cassette { interactions })
        .await
        .unwrap()
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn jwt(payload: &Value) -> String {
    let encode =
        |value: &Value| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.to_string());
    format!(
        "{}.{}.sig",
        encode(&json!({"alg": "none"})),
        encode(payload)
    )
}

fn query(url: &str, name: &str) -> String {
    url::Url::parse(url)
        .unwrap()
        .query_pairs()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
        .unwrap()
}

fn body(request: &ri_mock::RecordedRequest) -> Vec<(String, String)> {
    url::form_urlencoded::parse(request.body.as_bytes())
        .into_owned()
        .collect()
}

type Events = Arc<Mutex<Vec<AuthEvent>>>;

/// Serves a sign-in like a UI: answers each prompt with `answer`, which sees
/// the events so far, and records every event.
fn serve(
    mut requests: UnboundedReceiver<AuthRequest>,
    mut answer: impl FnMut(&AuthPrompt, &[AuthEvent], CancellationToken) -> Option<String>
    + Send
    + 'static,
) -> Events {
    let events: Events = Arc::default();
    let seen = Arc::clone(&events);
    tokio::spawn(async move {
        while let Some(request) = requests.recv().await {
            match request {
                AuthRequest::Notify(event) => seen.lock().unwrap().push(event),
                AuthRequest::Prompt {
                    prompt,
                    reply,
                    cancel,
                } => {
                    let events = seen.lock().unwrap().clone();
                    if let Some(text) = answer(&prompt, &events, cancel) {
                        let _ = reply.send(text);
                    } else {
                        // Keep the prompt open until the sign-in withdraws it.
                        tokio::spawn(async move {
                            let mut reply = reply;
                            reply.closed().await;
                        });
                    }
                }
            }
        }
    });
    events
}

fn auth_url(events: &[AuthEvent]) -> String {
    events
        .iter()
        .find_map(|event| match event {
            AuthEvent::AuthUrl { url, .. } => Some(url.clone()),
            _ => None,
        })
        .unwrap()
}

#[tokio::test]
async fn anthropic_copy_code_login_and_refresh() {
    let server = mock(vec![
        exchange(
            "POST",
            "/v1/oauth/token",
            200,
            &json!({"access_token": "sk-ant-oat-1", "refresh_token": "r1", "expires_in": 3600}),
        ),
        exchange(
            "POST",
            "/v1/oauth/token",
            200,
            &json!({"access_token": "sk-ant-oat-2", "refresh_token": "r2", "expires_in": 3600}),
        ),
    ])
    .await;
    let oauth = AnthropicOAuth {
        token_url: format!("{}/v1/oauth/token", server.url()),
        ..AnthropicOAuth::default()
    };
    let (interaction, requests) = Interaction::new(CancellationToken::new());
    let events = serve(requests, |prompt, events, _| match prompt {
        AuthPrompt::Select { options, .. } => Some(options[1].id.clone()),
        AuthPrompt::ManualCode { .. } => {
            let state = query(&auth_url(events), "state");
            Some(format!("the-code#{state}"))
        }
        _ => None,
    });
    let credential = oauth
        .login(&interaction, &LoginOptions::default())
        .await
        .unwrap();
    assert_eq!(credential.access, "sk-ant-oat-1");
    assert_eq!(credential.refresh, "r1");
    let url = auth_url(&events.lock().unwrap());
    assert!(url.starts_with("https://claude.ai/oauth/authorize?code=true&client_id="));
    assert_eq!(
        query(&url, "redirect_uri"),
        "https://platform.claude.com/oauth/code/callback"
    );
    let verifier = query(&url, "state");
    assert_eq!(
        query(&url, "code_challenge"),
        ri_ai::auth::pkce::challenge(&verifier)
    );

    let refreshed = oauth
        .refresh(&credential, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(refreshed.access, "sk-ant-oat-2");
    let requests = server.finish().unwrap();
    let exchange: Value = serde_json::from_str(&requests[0].body).unwrap();
    assert_eq!(
        exchange,
        json!({
            "grant_type": "authorization_code",
            "client_id": "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
            "code": "the-code",
            "state": verifier,
            "redirect_uri": "https://platform.claude.com/oauth/code/callback",
            "code_verifier": verifier,
        })
    );
    let refresh: Value = serde_json::from_str(&requests[1].body).unwrap();
    assert_eq!(refresh["grant_type"], "refresh_token");
    assert_eq!(refresh["refresh_token"], "r1");
}

#[tokio::test]
async fn anthropic_browser_login_takes_the_callback() {
    let server = mock(vec![exchange(
        "POST",
        "/v1/oauth/token",
        200,
        &json!({"access_token": "sk-ant-oat-b", "refresh_token": "rb", "expires_in": 60}),
    )])
    .await;
    let port = free_port();
    let oauth = AnthropicOAuth {
        token_url: format!("{}/v1/oauth/token", server.url()),
        callback_port: port,
        redirect_uri: format!("http://localhost:{port}/callback"),
        ..AnthropicOAuth::default()
    };
    let (interaction, requests) = Interaction::new(CancellationToken::new());
    let withdrawn = Arc::new(Mutex::new(None));
    let withdrawn_seen = Arc::clone(&withdrawn);
    serve(requests, move |prompt, events, cancel| match prompt {
        AuthPrompt::Select { options, .. } => Some(options[0].id.clone()),
        AuthPrompt::ManualCode { .. } => {
            let state = query(&auth_url(events), "state");
            *withdrawn_seen.lock().unwrap() = Some(cancel);
            tokio::spawn(async move {
                let url = format!("http://127.0.0.1:{port}/callback?code=cb-code&state={state}");
                let response = reqwest::get(url).await.unwrap();
                assert_eq!(response.status().as_u16(), 200);
            });
            None
        }
        _ => None,
    });
    let credential = oauth
        .login(&interaction, &LoginOptions::default())
        .await
        .unwrap();
    assert_eq!(credential.access, "sk-ant-oat-b");
    assert!(withdrawn.lock().unwrap().as_ref().unwrap().is_cancelled());
    let requests = server.finish().unwrap();
    let exchange: Value = serde_json::from_str(&requests[0].body).unwrap();
    assert_eq!(exchange["code"], "cb-code");
    assert_eq!(
        exchange["redirect_uri"],
        format!("http://localhost:{port}/callback")
    );
}

#[tokio::test]
async fn codex_device_code_login() {
    let access = jwt(&json!({"https://api.openai.com/auth": {"chatgpt_account_id": "acct_9"}}));
    let server = mock(vec![
        exchange(
            "POST",
            "/api/accounts/deviceauth/usercode",
            200,
            &json!({"device_auth_id": "dev-1", "user_code": "ABCD-1234", "interval": "0"}),
        ),
        exchange("POST", "/api/accounts/deviceauth/token", 403, &json!({})),
        exchange(
            "POST",
            "/api/accounts/deviceauth/token",
            200,
            &json!({"authorization_code": "ac", "code_verifier": "cv"}),
        ),
        exchange(
            "POST",
            "/oauth/token",
            200,
            &json!({"access_token": access, "refresh_token": "rt", "expires_in": 100}),
        ),
    ])
    .await;
    let oauth = CodexOAuth {
        auth_base_url: server.url(),
        ..CodexOAuth::default()
    };
    let (interaction, requests) = Interaction::new(CancellationToken::new());
    let events = serve(requests, |prompt, _, _| match prompt {
        AuthPrompt::Select { options, .. } => Some(options[1].id.clone()),
        _ => None,
    });
    let credential = oauth
        .login(&interaction, &LoginOptions::default())
        .await
        .unwrap();
    assert_eq!(credential.extra["accountId"], "acct_9");
    assert_eq!(credential.refresh, "rt");
    let device = events
        .lock()
        .unwrap()
        .iter()
        .find_map(|event| match event {
            AuthEvent::DeviceCode {
                user_code,
                verification_uri,
                ..
            } => Some((user_code.clone(), verification_uri.clone())),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        device,
        (
            "ABCD-1234".to_owned(),
            format!("{}/codex/device", server.url())
        )
    );
    let requests = server.finish().unwrap();
    let token = body(&requests[3]);
    assert!(token.contains(&("code_verifier".into(), "cv".into())));
    assert!(token.contains(&(
        "redirect_uri".into(),
        format!("{}/deviceauth/callback", server.url())
    )));
}

#[tokio::test]
async fn copilot_device_flow_enables_policy_models() {
    let known = ri_ai::catalog::builtin_models("github-copilot")[0]
        .id
        .clone();
    let server = mock(vec![
        exchange(
            "POST",
            "/login/device/code",
            200,
            &json!({"device_code": "dc", "user_code": "WXYZ", "verification_uri": "https://github.com/login/device", "interval": 0, "expires_in": 30}),
        ),
        exchange(
            "POST",
            "/login/oauth/access_token",
            200,
            &json!({"error": "authorization_pending"}),
        ),
        exchange(
            "POST",
            "/login/oauth/access_token",
            200,
            &json!({"access_token": "gho_token"}),
        ),
        exchange(
            "GET",
            "/copilot_internal/v2/token",
            200,
            &json!({"token": "tid=1;exp=2", "expires_at": 4_000_000_000_u64}),
        ),
        exchange(
            "GET",
            "/models",
            200,
            &json!({"data": [
                {"id": "picked", "model_picker_enabled": true},
                {"id": known, "model_picker_enabled": true, "policy": {"state": "unconfigured"}},
            ]}),
        ),
        exchange(
            "POST",
            &format!("/models/{known}/policy"),
            200,
            &json!({}),
        ),
    ])
    .await;
    let oauth = CopilotOAuth {
        github_url: Some(server.url()),
        api_url: Some(server.url()),
    };
    let (interaction, requests) = Interaction::new(CancellationToken::new());
    serve(requests, |prompt, _, _| match prompt {
        AuthPrompt::Text { .. } => Some(String::new()),
        _ => None,
    });
    let credential = oauth
        .login(&interaction, &LoginOptions::default())
        .await
        .unwrap();
    assert_eq!(credential.refresh, "gho_token");
    assert_eq!(credential.access, "tid=1;exp=2");
    assert_eq!(credential.expires, 4_000_000_000_000 - 300_000);
    assert_eq!(
        credential.extra["availableModelIds"],
        json!(["picked", known])
    );
    let requests = server.finish().unwrap();
    assert_eq!(
        requests[3]
            .headers
            .get("editor-version")
            .map(String::as_str),
        Some("vscode/1.107.0")
    );
    assert_eq!(requests[5].body, "{\"state\":\"enabled\"}");
}

#[tokio::test]
async fn chatgpt_login_registers_a_client() {
    let server = mock(vec![exchange(
        "POST",
        "/api/accounts/oauth/token",
        200,
        &json!({
            "access_token": "at", "refresh_token": "rt", "id_token": "it", "expires_in": 600,
            "scope": "openid chatgpt.tokens.use.direct",
        }),
    )])
    .await;
    let port = free_port();
    let oauth = ChatGptOAuth {
        token_url: format!("{}/api/accounts/oauth/token", server.url()),
        callback_port: port,
        redirect_uri: format!("http://127.0.0.1:{port}/auth/callback"),
        ..ChatGptOAuth::default()
    };
    let (interaction, requests) = Interaction::new(CancellationToken::new());
    serve(requests, move |prompt, events, _| match prompt {
        AuthPrompt::ManualCode { .. } => {
            let state = query(&auth_url(events), "state");
            Some(format!(
                "http://127.0.0.1:{port}/auth/callback?code=c&state={state}&client_id=issued"
            ))
        }
        _ => None,
    });
    let options = LoginOptions {
        device_id: Some("123E4567-e89b-12d3-a456-426614174000".into()),
    };
    let credential = oauth.login(&interaction, &options).await.unwrap();
    assert_eq!(credential.extra["clientId"], "issued");
    assert_eq!(
        credential.extra["scopes"],
        json!(["openid", "chatgpt.tokens.use.direct"])
    );
    let requests = server.finish().unwrap();
    let token = body(&requests[0]);
    assert!(token.contains(&("client_id".into(), "issued".into())));
    assert!(token.contains(&("resource".into(), "https://api.openai.com/v1".into())));
    let missing = oauth
        .login(&interaction, &LoginOptions::default())
        .await
        .unwrap_err();
    assert_eq!(
        missing.to_string(),
        "Sign in with ChatGPT requires a device ID (UUID) for this installation"
    );
}

fn agent_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("ri-oauth-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test]
async fn registry_refreshes_expired_tokens_and_persists_them() {
    let server = mock(vec![exchange(
        "POST",
        "/v1/oauth/token",
        200,
        &json!({"access_token": "sk-ant-oat-new", "refresh_token": "r-new", "expires_in": 3600}),
    )])
    .await;
    let dir = agent_dir("refresh");
    let original = "{\n  \"openai\": {\n    \"type\": \"api_key\",\n    \"key\": \"sk-x\"\n  },\n  \"anthropic\": {\n    \"type\": \"oauth\",\n    \"refresh\": \"r-old\",\n    \"access\": \"sk-ant-oat-old\",\n    \"expires\": 1\n  }\n}";
    std::fs::write(dir.join("auth.json"), original).unwrap();
    let mut registry = ModelRegistry::load(&dir);
    registry.register_oauth(
        "anthropic",
        Arc::new(AnthropicOAuth {
            token_url: format!("{}/v1/oauth/token", server.url()),
            ..AnthropicOAuth::default()
        }),
    );
    let model = registry
        .find("anthropic", "claude-sonnet-4-5")
        .unwrap()
        .clone();
    let auth = registry.auth(&model).await;
    assert_eq!(auth.api_key.as_deref(), Some("sk-ant-oat-new"));
    assert_eq!(auth.source.as_deref(), Some("OAuth"));
    // A second request uses the stored token without refreshing again.
    let again = registry.auth(&model).await;
    assert_eq!(again.api_key.as_deref(), Some("sk-ant-oat-new"));
    server.finish().unwrap();

    let text = std::fs::read_to_string(dir.join("auth.json")).unwrap();
    assert!(text.starts_with("{\n  \"openai\": {\n    \"type\": \"api_key\",\n    \"key\": \"sk-x\"\n  },\n  \"anthropic\": {"));
    let Some(Credential::OAuth(stored)) = registry.store().get("anthropic") else {
        panic!("expected an OAuth credential");
    };
    assert_eq!(
        (stored.access.as_str(), stored.refresh.as_str()),
        ("sk-ant-oat-new", "r-new")
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn registry_reports_refresh_failures_and_logs_in_with_keys() {
    let server = mock(vec![exchange(
        "POST",
        "/v1/oauth/token",
        400,
        &json!({"error": "invalid_grant"}),
    )])
    .await;
    let dir = agent_dir("failure");
    let mut registry = ModelRegistry::load(&dir);
    let token_url = format!("{}/v1/oauth/token", server.url());
    registry.register_oauth(
        "anthropic",
        Arc::new(AnthropicOAuth {
            token_url: token_url.clone(),
            ..AnthropicOAuth::default()
        }),
    );
    let cancel = CancellationToken::new();
    registry
        .store()
        .set(
            "anthropic",
            Credential::OAuth(OAuthCredential {
                access: "a".into(),
                refresh: "r".into(),
                expires: 0,
                extra: serde_json::Map::new(),
            }),
            &cancel,
        )
        .await
        .unwrap();
    let model = registry
        .find("anthropic", "claude-sonnet-4-5")
        .unwrap()
        .clone();
    let auth = registry.auth(&model).await;
    assert_eq!(
        auth.error.as_deref(),
        Some(
            format!(
                "OAuth refresh failed for anthropic: Anthropic token refresh request failed. url={token_url}; details=Error: HTTP request failed. status=400; url={token_url}; body={{\"error\":\"invalid_grant\"}}"
            )
            .as_str()
        )
    );

    let (interaction, requests) = Interaction::new(CancellationToken::new());
    serve(requests, |prompt, _, _| match prompt {
        AuthPrompt::Secret { message } => {
            assert_eq!(message, "Enter OpenAI API key");
            Some("sk-new".into())
        }
        _ => None,
    });
    registry
        .login(
            "openai",
            LoginKind::ApiKey,
            &interaction,
            &LoginOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        registry.auth_source("openai").as_deref(),
        Some("stored credential")
    );
    registry.logout("openai").await.unwrap();
    assert!(registry.store().get("openai").is_none());
    std::fs::remove_dir_all(&dir).unwrap();
}
