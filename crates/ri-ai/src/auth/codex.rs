//! OpenAI Codex (ChatGPT Plus/Pro) sign-in: PKCE with a loopback callback, or
//! a device code. Port of `oauth/openai-codex.ts`.

use base64::Engine as _;
use ri_types::auth::OAuthCredential;
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use super::anthropic::expiry;
use super::callback::{Received, callback_or_manual, start_code_server};
use super::device::{Poll, poll_device_code};
use super::{
    AuthError, AuthEvent, AuthPrompt, BoxFuture, Interaction, LoginOptions, OAuthProvider,
    SelectOption, callback_host, error_text, form, parse_authorization_input, pkce, send,
};

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const SCOPE: &str = "openid profile email offline_access";
const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";
const DEVICE_CODE_TIMEOUT_SECONDS: f64 = 15.0 * 60.0;
const BROWSER: &str = "browser";
const DEVICE_CODE: &str = "device_code";
/// The originator OpenAI expects from this client id; pi sends its own name.
const ORIGINATOR: &str = "pi";

/// The Codex sign-in and its endpoints.
#[derive(Clone, Debug)]
pub struct CodexOAuth {
    /// `https://auth.openai.com`; every endpoint is relative to it.
    pub auth_base_url: String,
    /// Loopback callback port, shared with the Codex CLI.
    pub callback_port: u16,
    /// Redirect URI for the browser flow; must reach the callback port.
    pub redirect_uri: String,
}

impl Default for CodexOAuth {
    fn default() -> CodexOAuth {
        CodexOAuth {
            auth_base_url: "https://auth.openai.com".into(),
            callback_port: 1455,
            redirect_uri: "http://localhost:1455/auth/callback".into(),
        }
    }
}

struct Token {
    access: String,
    refresh: String,
    expires: u64,
}

/// The JSON payload of a JWT, or `None`.
pub(crate) fn decode_jwt(token: &str) -> Option<Value> {
    let parts: Vec<&str> = token.split('.').collect();
    let [_, payload, _] = parts.as_slice() else {
        return None;
    };
    let trimmed = payload.trim_end_matches('=');
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(trimmed)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(trimmed))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn account_id(access: &str) -> Option<String> {
    decode_jwt(access)?
        .get(JWT_CLAIM_PATH)?
        .get("chatgpt_account_id")?
        .as_str()
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

fn credential(token: Token) -> Result<OAuthCredential, AuthError> {
    let account_id = account_id(&token.access)
        .ok_or_else(|| AuthError::failed("Failed to extract accountId from token"))?;
    let mut extra = Map::new();
    extra.insert("accountId".into(), Value::String(account_id));
    Ok(OAuthCredential {
        access: token.access,
        refresh: token.refresh,
        expires: token.expires,
        extra,
    })
}

impl CodexOAuth {
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.auth_base_url.trim_end_matches('/'))
    }

    async fn read_token(response: reqwest::Response, operation: &str) -> Result<Token, AuthError> {
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = error_text(response).await;
            return Err(AuthError::Failed(format!(
                "OpenAI Codex token {operation} failed ({status}): {text}"
            )));
        }
        let json: Value = super::json_body(response).await;
        let field = |name: &str| {
            json.get(name)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        };
        match (
            field("access_token"),
            field("refresh_token"),
            json.get("expires_in").and_then(Value::as_f64),
        ) {
            (Some(access), Some(refresh), Some(expires_in)) => Ok(Token {
                access: access.to_owned(),
                refresh: refresh.to_owned(),
                expires: expiry(expires_in, 0.0),
            }),
            _ => Err(AuthError::Failed(format!(
                "OpenAI Codex token {operation} response missing fields: {}",
                ri_types::json::to_string(&json).unwrap_or_default()
            ))),
        }
    }

    async fn exchange(
        &self,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
        cancel: &CancellationToken,
    ) -> Result<OAuthCredential, AuthError> {
        let request = crate::http::client()
            .post(self.url("/oauth/token"))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(form(&[
                ("grant_type", "authorization_code"),
                ("client_id", CLIENT_ID),
                ("code", code),
                ("code_verifier", verifier),
                ("redirect_uri", redirect_uri),
            ]));
        let response = send(request, cancel).await?;
        credential(Self::read_token(response, "exchange").await?)
    }

    fn authorize_url(&self, challenge: &str, state: &str) -> String {
        let query = form(&[
            ("response_type", "code"),
            ("client_id", CLIENT_ID),
            ("redirect_uri", &self.redirect_uri),
            ("scope", SCOPE),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
            ("state", state),
            ("id_token_add_organizations", "true"),
            ("codex_cli_simplified_flow", "true"),
            ("originator", ORIGINATOR),
        ]);
        format!("{}?{query}", self.url("/oauth/authorize"))
    }

    async fn login_browser(&self, interaction: &Interaction) -> Result<OAuthCredential, AuthError> {
        let pkce = pkce::generate();
        let mut state_bytes = [0u8; 16];
        let _ = getrandom::fill(&mut state_bytes);
        let state: String = state_bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        // Port 1455 is shared with the Codex CLI; when it is taken, the pasted redirect URL is used.
        let mut server = start_code_server(
            "OpenAI",
            &callback_host(),
            self.callback_port,
            "/auth/callback",
            Some(state.clone()),
            interaction.cancel(),
        )
        .await
        .ok();
        interaction.notify(AuthEvent::AuthUrl {
            url: self.authorize_url(&pkce.challenge, &state),
            instructions: Some("A browser window should open. Complete login to finish.".into()),
        });
        let received = callback_or_manual(
            interaction,
            server.as_mut(),
            "Complete login in your browser, or paste the authorization code / redirect URL here:",
            &self.redirect_uri,
        )
        .await?;
        drop(server);
        let code = match received {
            Received::Callback(code) => Some(code),
            Received::Manual(input) => {
                let (code, pasted_state) = parse_authorization_input(&input);
                if pasted_state.is_some_and(|pasted| pasted != state) {
                    return Err(AuthError::failed("State mismatch"));
                }
                code
            }
        };
        let code = code.ok_or_else(|| AuthError::failed("Missing authorization code"))?;
        self.exchange(
            &code,
            &pkce.verifier,
            &self.redirect_uri,
            interaction.cancel(),
        )
        .await
    }

    async fn login_device_code(
        &self,
        interaction: &Interaction,
    ) -> Result<OAuthCredential, AuthError> {
        let cancel = interaction.cancel();
        let request = crate::http::client()
            .post(self.url("/api/accounts/deviceauth/usercode"))
            .header("Content-Type", "application/json")
            .body(
                ri_types::json::to_string(&super::object(&[("client_id", CLIENT_ID)]))
                    .unwrap_or_default(),
            );
        let response = send(request, cancel).await?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            if status == 404 {
                return Err(AuthError::failed(
                    "OpenAI Codex device code login is not enabled for this server. Use browser login or verify the server URL.",
                ));
            }
            let body = response.text().await.unwrap_or_default();
            let suffix = if body.is_empty() {
                String::new()
            } else {
                format!(": {body}")
            };
            return Err(AuthError::Failed(format!(
                "OpenAI Codex device code request failed with status {status}{suffix}"
            )));
        }
        let json: Value = super::json_body(response).await;
        let interval = match json.get("interval") {
            Some(Value::String(text)) => text.trim().parse::<f64>().ok(),
            Some(value) => value.as_f64(),
            None => None,
        };
        let device_auth_id = json
            .get("device_auth_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let user_code = json
            .get("user_code")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let (Some(device_auth_id), Some(user_code), Some(interval)) = (
            device_auth_id,
            user_code,
            interval.filter(|value| value.is_finite() && *value >= 0.0),
        ) else {
            return Err(AuthError::Failed(format!(
                "Invalid OpenAI Codex device code response: {}",
                ri_types::json::to_string(&json).unwrap_or_default()
            )));
        };
        interaction.notify(AuthEvent::DeviceCode {
            user_code: user_code.to_owned(),
            verification_uri: self.url("/codex/device"),
            interval_seconds: Some(interval),
            expires_in_seconds: Some(DEVICE_CODE_TIMEOUT_SECONDS),
        });
        let poll_body = ri_types::json::to_string(&super::object(&[
            ("device_auth_id", device_auth_id),
            ("user_code", user_code),
        ]))
        .unwrap_or_default();
        let token_url = self.url("/api/accounts/deviceauth/token");
        let (code, verifier) = poll_device_code(
            Some(interval),
            Some(DEVICE_CODE_TIMEOUT_SECONDS),
            false,
            cancel,
            || {
                let request = crate::http::client()
                    .post(&token_url)
                    .header("Content-Type", "application/json")
                    .body(poll_body.clone());
                async move {
                    let response = send(request, cancel).await?;
                    let status = response.status().as_u16();
                    if response.status().is_success() {
                        let json: Value = super::json_body(response).await;
                        let code = json.get("authorization_code").and_then(Value::as_str);
                        let verifier = json.get("code_verifier").and_then(Value::as_str);
                        return Ok(match (code, verifier) {
                            (Some(code), Some(verifier))
                                if !code.is_empty() && !verifier.is_empty() =>
                            {
                                Poll::Complete((code.to_owned(), verifier.to_owned()))
                            }
                            _ => Poll::Failed(format!(
                                "Invalid OpenAI Codex device auth token response: {}",
                                ri_types::json::to_string(&json).unwrap_or_default()
                            )),
                        });
                    }
                    if status == 403 || status == 404 {
                        return Ok(Poll::Pending);
                    }
                    let body = response.text().await.unwrap_or_default();
                    let code =
                        serde_json::from_str::<Value>(&body).ok().and_then(|json| {
                            match json.get("error")? {
                                Value::String(code) => Some(code.clone()),
                                Value::Object(error) => {
                                    error.get("code")?.as_str().map(str::to_owned)
                                }
                                _ => None,
                            }
                        });
                    Ok(match code.as_deref() {
                        Some("deviceauth_authorization_pending") => Poll::Pending,
                        Some("slow_down") => Poll::SlowDown(None),
                        _ => {
                            let suffix = if body.is_empty() {
                                String::new()
                            } else {
                                format!(": {body}")
                            };
                            Poll::Failed(format!(
                                "OpenAI Codex device auth failed with status {status}{suffix}"
                            ))
                        }
                    })
                }
            },
        )
        .await?;
        self.exchange(&code, &verifier, &self.url("/deviceauth/callback"), cancel)
            .await
    }

    async fn refresh_token(
        &self,
        refresh: &str,
        cancel: &CancellationToken,
    ) -> Result<OAuthCredential, AuthError> {
        let request = crate::http::client()
            .post(self.url("/oauth/token"))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh),
                ("client_id", CLIENT_ID),
            ]));
        let response = send(request, cancel).await.map_err(|err| match err {
            AuthError::Failed(message) => {
                AuthError::Failed(format!("OpenAI Codex token refresh error: {message}"))
            }
            cancelled => cancelled,
        })?;
        credential(Self::read_token(response, "refresh").await?)
    }
}

impl OAuthProvider for CodexOAuth {
    fn name(&self) -> &str {
        "OpenAI (ChatGPT Plus/Pro)"
    }

    fn login<'a>(
        &'a self,
        interaction: &'a Interaction,
        _options: &'a LoginOptions,
    ) -> BoxFuture<'a, Result<OAuthCredential, AuthError>> {
        Box::pin(async move {
            let method = interaction
                .prompt(AuthPrompt::Select {
                    message: "Select OpenAI Codex login method:".into(),
                    options: vec![
                        SelectOption {
                            id: BROWSER.into(),
                            label: "Browser login (default)".into(),
                        },
                        SelectOption {
                            id: DEVICE_CODE.into(),
                            label: "Device code login (headless)".into(),
                        },
                    ],
                })
                .await?;
            match method.as_str() {
                DEVICE_CODE => self.login_device_code(interaction).await,
                BROWSER => self.login_browser(interaction).await,
                other => Err(AuthError::Failed(format!(
                    "Unknown OpenAI Codex login method: {other}"
                ))),
            }
        })
    }

    fn refresh<'a>(
        &'a self,
        credential: &'a OAuthCredential,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<OAuthCredential, AuthError>> {
        Box::pin(self.refresh_token(&credential.refresh, cancel))
    }
}
