//! OAuth sign-ins and refresh against a mock authorization server.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers; a panic is a test failure"
)]

use std::sync::{Arc, Mutex};

use base64::Engine as _;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_util::sync::CancellationToken;
use yapi_ai::auth::anthropic::AnthropicOAuth;
use yapi_ai::auth::chatgpt::ChatGptOAuth;
use yapi_ai::auth::codex::CodexOAuth;
use yapi_ai::auth::copilot::CopilotOAuth;
use yapi_ai::auth::kimi::KimiOAuth;
use yapi_ai::auth::meta::MetaOAuth;
use yapi_ai::auth::openrouter::OpenRouterOAuth;
use yapi_ai::auth::radius::RadiusOAuth;
use yapi_ai::auth::xai::XaiOAuth;
use yapi_ai::auth::{AuthEvent, AuthPrompt, AuthRequest, Interaction, LoginOptions, OAuthProvider};
use yapi_ai::registry::{LoginKind, ModelRegistry};
use yapi_mock::{Cassette, Interaction as Exchange, MockServer};
use yapi_types::auth::{Credential, OAuthCredential};

async fn mock(interactions: Vec<Exchange>) -> MockServer {
    MockServer::local(Cassette { interactions }).await.unwrap()
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

fn body(request: &yapi_mock::RecordedRequest) -> Vec<(String, String)> {
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
        Exchange::json(
            "POST",
            "/v1/oauth/token",
            200,
            &json!({"access_token": "sk-ant-oat-1", "refresh_token": "r1", "expires_in": 3600}),
        ),
        Exchange::json(
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
        yapi_ai::auth::pkce::challenge(&verifier)
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
    let server = mock(vec![Exchange::json(
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
        Exchange::json(
            "POST",
            "/api/accounts/deviceauth/usercode",
            200,
            &json!({"device_auth_id": "dev-1", "user_code": "ABCD-1234", "interval": "0"}),
        ),
        Exchange::json("POST", "/api/accounts/deviceauth/token", 403, &json!({})),
        Exchange::json(
            "POST",
            "/api/accounts/deviceauth/token",
            200,
            &json!({"authorization_code": "ac", "code_verifier": "cv"}),
        ),
        Exchange::json(
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
    let known = yapi_ai::catalog::builtin_models("github-copilot")[0]
        .id
        .clone();
    let server = mock(vec![
        Exchange::json(
            "POST",
            "/login/device/code",
            200,
            &json!({"device_code": "dc", "user_code": "WXYZ", "verification_uri": "https://github.com/login/device", "interval": 0, "expires_in": 30}),
        ),
        Exchange::json(
            "POST",
            "/login/oauth/access_token",
            200,
            &json!({"error": "authorization_pending"}),
        ),
        Exchange::json(
            "POST",
            "/login/oauth/access_token",
            200,
            &json!({"access_token": "gho_token"}),
        ),
        Exchange::json(
            "GET",
            "/copilot_internal/v2/token",
            200,
            &json!({"token": "tid=1;exp=2", "expires_at": 4_000_000_000_u64}),
        ),
        Exchange::json(
            "GET",
            "/models",
            200,
            &json!({"data": [
                {"id": "picked", "model_picker_enabled": true},
                {"id": known, "model_picker_enabled": true, "policy": {"state": "unconfigured"}},
            ]}),
        ),
        Exchange::json(
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
    let server = mock(vec![Exchange::json(
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
    let dir = std::env::temp_dir().join(format!("yapi-oauth-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A sign-in that refreshes to `access-2` and cannot derive a key.
#[derive(Default)]
struct NoKey {
    keys_asked: Mutex<u32>,
}

impl OAuthProvider for NoKey {
    fn name(&self) -> &str {
        "No key"
    }

    fn login<'a>(
        &'a self,
        _interaction: &'a Interaction,
        _options: &'a LoginOptions,
    ) -> futures_util::future::BoxFuture<'a, Result<OAuthCredential, yapi_ai::auth::AuthError>>
    {
        Box::pin(async { Err(yapi_ai::auth::AuthError::Cancelled) })
    }

    fn refresh<'a>(
        &'a self,
        credential: &'a OAuthCredential,
        _cancel: &'a CancellationToken,
    ) -> futures_util::future::BoxFuture<'a, Result<OAuthCredential, yapi_ai::auth::AuthError>>
    {
        Box::pin(async move {
            Ok(OAuthCredential {
                access: "access-2".into(),
                expires: u64::MAX,
                ..credential.clone()
            })
        })
    }

    fn to_auth<'a>(
        &'a self,
        _credential: &'a OAuthCredential,
    ) -> futures_util::future::BoxFuture<
        'a,
        Result<yapi_ai::auth::OAuthAuth, yapi_ai::auth::AuthError>,
    > {
        *self.keys_asked.lock().unwrap() += 1;
        Box::pin(async { Err(yapi_ai::auth::AuthError::Failed("no key".into())) })
    }
}

/// A catalog refresh's credential is the refreshed OAuth credential, as in
/// pi, which derives no key for it: a sign-in that cannot derive one still
/// refreshes.
#[tokio::test]
async fn refresh_credentials_refresh_oauth_without_deriving_a_key() {
    let dir = agent_dir("refresh-credential");
    std::fs::write(
        dir.join("auth.json"),
        r#"{"sso": {"type": "oauth", "refresh": "r", "access": "access-1", "expires": 1}}"#,
    )
    .unwrap();
    let mut registry = ModelRegistry::load(&dir);
    let flow = Arc::new(NoKey::default());
    registry.register_oauth("sso", flow.clone());
    let Some(Credential::OAuth(credential)) = registry.refresh_credential("sso").await else {
        panic!("no refreshed credential");
    };
    assert_eq!(credential.access, "access-2");
    assert_eq!(*flow.keys_asked.lock().unwrap(), 0);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn registry_refreshes_expired_tokens_and_persists_them() {
    let server = mock(vec![Exchange::json(
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
    let server = mock(vec![Exchange::json(
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
    assert_eq!(registry.login_status("openai").as_deref(), Some("stored"));
    registry.logout("openai").await.unwrap();
    assert!(registry.store().get("openai").is_none());
    std::fs::remove_dir_all(&dir).unwrap();
}

fn device_code(events: &Events) -> (String, String) {
    events
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
        .unwrap()
}

#[tokio::test]
async fn kimi_device_login_refresh_and_bearer_header() {
    let server = mock(vec![
        Exchange::json(
            "POST",
            "/api/oauth/device_authorization",
            200,
            &json!({"device_code": "dc", "user_code": "KIMI-1", "verification_uri": "https://kimi.com/device", "verification_uri_complete": "https://kimi.com/device?code=KIMI-1", "interval": 1, "expires_in": 60}),
        ),
        Exchange::json("POST", "/api/oauth/token", 400, &json!({"error": "authorization_pending"})),
        Exchange::json(
            "POST",
            "/api/oauth/token",
            200,
            &json!({"access_token": "kimi-access", "refresh_token": "kimi-refresh", "expires_in": 3600}),
        ),
        Exchange::json(
            "POST",
            "/api/oauth/token",
            401,
            &json!({"error": "invalid_grant", "error_description": "expired"}),
        ),
    ])
    .await;
    let oauth = KimiOAuth {
        oauth_host: Some(server.url()),
    };
    let (interaction, requests) = Interaction::new(CancellationToken::new());
    let events = serve(requests, |_, _, _| None);
    let credential = oauth
        .login(&interaction, &LoginOptions::default())
        .await
        .unwrap();
    assert_eq!(
        device_code(&events),
        (
            "KIMI-1".into(),
            "https://kimi.com/device?code=KIMI-1".into()
        )
    );
    assert_eq!(
        (credential.access.as_str(), credential.refresh.as_str()),
        ("kimi-access", "kimi-refresh")
    );
    let auth = oauth.to_auth(&credential).await.unwrap();
    assert_eq!(auth.api_key, None);
    assert_eq!(
        auth.headers.get("Authorization"),
        Some(&Some("Bearer kimi-access".to_owned()))
    );
    let refused = oauth
        .refresh(&credential, &CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(
        refused.to_string(),
        "Kimi Code token refresh unauthorized (status 401): expired"
    );
    let requests = server.finish().unwrap();
    assert!(body(&requests[2]).contains(&(
        "grant_type".into(),
        "urn:ietf:params:oauth:grant-type:device_code".into()
    )));
    assert!(body(&requests[3]).contains(&("refresh_token".into(), "kimi-refresh".into())));
}

#[tokio::test]
async fn meta_device_login_mints_an_api_key() {
    let server = mock(vec![
        Exchange::json(
            "POST",
            "/oidc/device/authorization/",
            200,
            &json!({"device_code": "dc", "user_code": "META-1", "verification_uri": "https://meta.com/device", "interval": 1}),
        ),
        Exchange::json(
            "POST",
            "/oidc/device/token/",
            200,
            &json!({"access_token": "identity"}),
        ),
        Exchange::json("POST", "/muse-code/key", 200, &json!({"api_key": "meta-key"})),
        Exchange::json("POST", "/muse-code/key", 403, &json!({"detail": "session expired"})),
    ])
    .await;
    let url = server.url();
    let oauth = MetaOAuth {
        device_authorization_url: format!("{url}/oidc/device/authorization/"),
        device_token_url: format!("{url}/oidc/device/token/"),
        api_key_mint_url: format!("{url}/muse-code/key"),
    };
    let (interaction, requests) = Interaction::new(CancellationToken::new());
    let events = serve(requests, |_, _, _| None);
    let credential = oauth
        .login(&interaction, &LoginOptions::default())
        .await
        .unwrap();
    assert_eq!(device_code(&events).0, "META-1");
    assert_eq!(
        (credential.access.as_str(), credential.refresh.as_str()),
        ("meta-key", "identity")
    );
    let expired = oauth
        .refresh(&credential, &CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(
        expired.to_string(),
        "Meta session expired (status 403). Run `/login meta` to sign in again.: session expired"
    );
    let requests = server.finish().unwrap();
    assert_eq!(requests[2].headers["x-api-version"], "1.0.0");
    assert_eq!(requests[2].body, "{}");
}

#[tokio::test]
async fn xai_device_login_and_refresh_keep_the_refresh_token() {
    let server = mock(vec![
        Exchange::json(
            "POST",
            "/oauth2/device/code",
            200,
            &json!({"device_code": "dc", "user_code": "XAI-1", "verification_uri": "https://accounts.x.ai/device", "expires_in": 60, "interval": 1}),
        ),
        Exchange::json(
            "POST",
            "/oauth2/token",
            200,
            &json!({"access_token": "xai-1", "refresh_token": "xr-1", "expires_in": 3600}),
        ),
        Exchange::json("POST", "/oauth2/token", 200, &json!({"access_token": "xai-2"})),
    ])
    .await;
    let oauth = XaiOAuth {
        device_code_url: format!("{}/oauth2/device/code", server.url()),
        token_url: format!("{}/oauth2/token", server.url()),
    };
    let (interaction, requests) = Interaction::new(CancellationToken::new());
    let events = serve(requests, |_, _, _| None);
    let credential = oauth
        .login(&interaction, &LoginOptions::default())
        .await
        .unwrap();
    assert_eq!(device_code(&events).1, "https://accounts.x.ai/device");
    let refreshed = oauth
        .refresh(&credential, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(refreshed.access, "xai-2");
    assert_eq!(refreshed.refresh, "xr-1");
    let requests = server.finish().unwrap();
    assert!(body(&requests[0]).contains(&("referrer".into(), "pi".into())));
}

#[tokio::test]
async fn openrouter_trades_a_pasted_code_for_a_key() {
    let server = mock(vec![Exchange::json(
        "POST",
        "/api/v1/auth/keys",
        200,
        &json!({"key": "sk-or-v1-key"}),
    )])
    .await;
    let oauth = OpenRouterOAuth {
        authorize_url: format!("{}/auth", server.url()),
        token_url: format!("{}/api/v1/auth/keys", server.url()),
    };
    let (interaction, requests) = Interaction::new(CancellationToken::new());
    let events = serve(requests, |prompt, _, _| match prompt {
        AuthPrompt::ManualCode { .. } => {
            Some("http://127.0.0.1:1/oauth/callback/x?code=or-code".into())
        }
        _ => None,
    });
    let credential = oauth
        .login(&interaction, &LoginOptions::default())
        .await
        .unwrap();
    assert_eq!(credential.access, "sk-or-v1-key");
    assert_eq!(credential.expires, 9_007_199_254_740_991);
    let url = auth_url(&events.lock().unwrap());
    let callback = query(&url, "callback_url");
    assert!(callback.starts_with("http://127.0.0.1:"), "{callback}");
    assert!(callback.contains("/oauth/callback/"));
    assert_eq!(query(&url, "code_challenge_method"), "S256");
    let requests = server.finish().unwrap();
    let exchange: Value = serde_json::from_str(&requests[0].body).unwrap();
    assert_eq!(exchange["code"], "or-code");
    assert_eq!(exchange["code_challenge_method"], "S256");
    assert!(!oauth.is_subscription());
}

#[tokio::test]
async fn radius_device_login_polls_until_authorized() {
    let server = mock(vec![
        Exchange::json(
            "POST",
            "/v1/oauth/device",
            200,
            &json!({"device_code": "dc", "user_code": "RAD-1", "verification_uri": "https://radius.pi.dev/device", "expires_in": 60, "interval": 1}),
        ),
        Exchange::json("POST", "/v1/oauth/token", 400, &json!({"error": "authorization_pending"})),
        Exchange::json(
            "POST",
            "/v1/oauth/token",
            200,
            &json!({"access_token": "rad-access", "refresh_token": "rad-refresh", "expires_in": 3600, "scope": "gateway offline_access"}),
        ),
        Exchange::json(
            "POST",
            "/v1/oauth/token",
            400,
            &json!({"error": "invalid_grant", "error_description": "revoked"}),
        ),
    ])
    .await;
    let oauth = RadiusOAuth::new("Radius", &format!("{}/", server.url()));
    let (interaction, requests) = Interaction::new(CancellationToken::new());
    let events = serve(requests, |prompt, _, _| match prompt {
        AuthPrompt::Select { options, .. } => Some(options[1].id.clone()),
        _ => None,
    });
    let credential = oauth
        .login(&interaction, &LoginOptions::default())
        .await
        .unwrap();
    assert_eq!(device_code(&events).0, "RAD-1");
    assert_eq!(credential.access, "rad-access");
    assert_eq!(credential.refresh, "rad-refresh");
    assert_eq!(credential.extra["scope"], "gateway offline_access");
    let refused = oauth
        .refresh(&credential, &CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(
        refused.to_string(),
        "Radius OAuth token request failed: invalid_grant: revoked"
    );
    let requests = server.finish().unwrap();
    assert_eq!(
        body(&requests[0]),
        [
            ("client_id".to_owned(), "pi-gateway".to_owned()),
            ("scope".to_owned(), "gateway offline_access".to_owned()),
        ]
    );
    assert!(body(&requests[2]).contains(&("device_code".into(), "dc".into())));
    assert!(body(&requests[3]).contains(&("refresh_token".into(), "rad-refresh".into())));
}

#[tokio::test]
async fn radius_browser_login_checks_the_state() {
    let server = mock(vec![
        Exchange::json(
            "GET",
            "/v1/oauth",
            200,
            &json!({"authorizationEndpoint": "https://radius.example/authorize"}),
        ),
        Exchange::json(
            "POST",
            "/v1/oauth/token",
            200,
            &json!({"access_token": "rad-access", "refresh_token": "rad-refresh", "expires_in": 3600}),
        ),
    ])
    .await;
    let mut oauth = RadiusOAuth::new("Radius", &server.url());
    oauth.callback_port = free_port();
    let (interaction, requests) = Interaction::new(CancellationToken::new());
    let events = serve(requests, |prompt, _, _| match prompt {
        AuthPrompt::Select { options, .. } => Some(options[0].id.clone()),
        _ => None,
    });
    let login = tokio::spawn(async move {
        oauth
            .login(&interaction, &LoginOptions::default())
            .await
            .map(|credential| credential.access)
    });
    let url = loop {
        let found = events.lock().unwrap().iter().find_map(|event| match event {
            AuthEvent::AuthUrl { url, .. } => Some(url.clone()),
            _ => None,
        });
        if let Some(url) = found {
            break url;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };
    assert!(
        url.starts_with("https://radius.example/authorize?response_type=code&client_id=pi-gateway")
    );
    assert_eq!(query(&url, "handoff"), "url");
    let redirect = query(&url, "redirect_uri");
    let state = query(&url, "state");
    let wrong = reqwest::get(format!("{redirect}?code=c1&state=other"))
        .await
        .unwrap();
    assert_eq!(wrong.status().as_u16(), 400);
    let right = reqwest::get(format!("{redirect}?code=c1&state={state}"))
        .await
        .unwrap();
    assert_eq!(right.status().as_u16(), 200);
    assert_eq!(login.await.unwrap().unwrap(), "rad-access");
    let requests = server.finish().unwrap();
    let exchange = body(&requests[1]);
    assert!(exchange.contains(&("code".into(), "c1".into())));
    assert!(exchange.contains(&("redirect_uri".into(), redirect)));
}

#[tokio::test]
async fn llama_sign_in_checks_the_server_and_stores_its_url() {
    let server = mock(vec![Exchange::json(
        "GET",
        "/models",
        200,
        &json!({"data": [{"id": "qwen", "status": {"value": "loaded"}}]}),
    )])
    .await;
    let url = format!("{}/v1/", server.url());
    let (interaction, requests) = Interaction::new(CancellationToken::new());
    serve(requests, move |prompt, _, _| match prompt {
        AuthPrompt::Text { message, .. } if message == "llama.cpp server URL" => Some(url.clone()),
        AuthPrompt::Secret { message } if message == "API key (optional)" => Some("  ".into()),
        _ => None,
    });
    let credential = yapi_ai::key_auth::login("llama.cpp", &interaction)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(credential.key, None);
    assert_eq!(credential.env.unwrap()["LLAMA_BASE_URL"], server.url());
    let requests = server.finish().unwrap();
    assert!(!requests[0].headers.contains_key("authorization"));
}
