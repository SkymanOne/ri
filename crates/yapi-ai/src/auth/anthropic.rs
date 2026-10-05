//! Anthropic (Claude Pro/Max) sign-in: PKCE with a loopback callback or a
//! copied code. Port of `oauth/anthropic.ts`.

use std::time::Duration;

use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;
use yapi_types::auth::OAuthCredential;

use super::callback::{Received, callback_or_manual, start_code_server};
use super::{
    AuthError, AuthEvent, AuthPrompt, BoxFuture, Interaction, OAuthProvider, SelectOption,
    callback_host, now_ms, parse_authorization_input, pkce,
};

/// Public client id, base64 in pi's source.
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
const BROWSER: &str = "browser";
const COPY_CODE: &str = "copy_code";
const EXCHANGING: &str = "Exchanging authorization code for tokens...";
const EXPIRY_MARGIN_MS: f64 = 5.0 * 60.0 * 1000.0;

/// The Anthropic sign-in and its endpoints.
#[derive(Clone, Debug)]
pub struct AnthropicOAuth {
    /// Authorization page.
    pub authorize_url: String,
    /// Token endpoint.
    pub token_url: String,
    /// Loopback callback port.
    pub callback_port: u16,
    /// Redirect URI for the browser flow; must reach the callback port.
    pub redirect_uri: String,
    /// Redirect URI for the copy-code flow.
    pub copy_code_redirect_uri: String,
}

impl Default for AnthropicOAuth {
    fn default() -> AnthropicOAuth {
        AnthropicOAuth {
            authorize_url: "https://claude.ai/oauth/authorize".into(),
            token_url: "https://platform.claude.com/v1/oauth/token".into(),
            callback_port: 53692,
            redirect_uri: "http://localhost:53692/callback".into(),
            copy_code_redirect_uri: "https://platform.claude.com/oauth/code/callback".into(),
        }
    }
}

struct Token {
    access: String,
    refresh: String,
    expires_in: f64,
}

fn credential(token: Token) -> OAuthCredential {
    OAuthCredential {
        access: token.access,
        refresh: token.refresh,
        expires: expiry(token.expires_in, EXPIRY_MARGIN_MS),
        extra: Map::new(),
    }
}

/// `Date.now() + expires_in * 1000 - margin`, in milliseconds.
pub(crate) fn expiry(expires_in: f64, margin_ms: f64) -> u64 {
    let value = now_ms() as f64 + expires_in * 1000.0 - margin_ms;
    if value.is_finite() && value > 0.0 {
        value as u64
    } else {
        0
    }
}

fn parse_token(body: &str) -> Result<Token, String> {
    let value: Value = serde_json::from_str(body).map_err(|err| format!("SyntaxError: {err}"))?;
    let field = |name: &str| value.get(name).and_then(Value::as_str).map(str::to_owned);
    match (
        field("access_token"),
        field("refresh_token"),
        value.get("expires_in").and_then(Value::as_f64),
    ) {
        (Some(access), Some(refresh), Some(expires_in)) => Ok(Token {
            access,
            refresh,
            expires_in,
        }),
        _ => Err("Error: token response is missing fields".into()),
    }
}

impl AnthropicOAuth {
    fn authorize_url(&self, challenge: &str, verifier: &str, redirect_uri: &str) -> String {
        let query = super::form(&[
            ("code", "true"),
            ("client_id", CLIENT_ID),
            ("response_type", "code"),
            ("redirect_uri", redirect_uri),
            ("scope", SCOPES),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
            ("state", verifier),
        ]);
        format!("{}?{query}", self.authorize_url)
    }

    /// POSTs JSON and returns the body, or pi's `HTTP request failed` message.
    async fn post_json(
        &self,
        body: &Value,
        cancel: &CancellationToken,
    ) -> Result<String, AuthError> {
        let body =
            yapi_types::json::to_string(body).map_err(|err| AuthError::failed(err.to_string()))?;
        let request = crate::http::client()
            .post(&self.token_url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .timeout(Duration::from_secs(30))
            .body(body);
        let response = super::send(request, cancel).await?;
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
        if !(200..300).contains(&status) {
            return Err(AuthError::Failed(format!(
                "HTTP request failed. status={status}; url={}; body={text}",
                self.token_url
            )));
        }
        Ok(text)
    }

    async fn exchange(
        &self,
        code: &str,
        state: &str,
        verifier: &str,
        redirect_uri: &str,
        cancel: &CancellationToken,
    ) -> Result<OAuthCredential, AuthError> {
        let body = super::object(&[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code),
            ("state", state),
            ("redirect_uri", redirect_uri),
            ("code_verifier", verifier),
        ]);
        let text = self.post_json(&body, cancel).await.map_err(|err| match err {
            AuthError::Cancelled => AuthError::Cancelled,
            AuthError::Failed(message) => AuthError::Failed(format!(
                "Token exchange request failed. url={}; redirect_uri={redirect_uri}; response_type=authorization_code; details=Error: {message}",
                self.token_url
            )),
        })?;
        parse_token(&text).map(credential).map_err(|details| {
            AuthError::Failed(format!(
                "Token exchange returned invalid JSON. url={}; body={text}; details={details}",
                self.token_url
            ))
        })
    }

    async fn login_browser(&self, interaction: &Interaction) -> Result<OAuthCredential, AuthError> {
        let pkce = pkce::generate();
        let mut server = start_code_server(
            "Anthropic",
            &callback_host(),
            self.callback_port,
            "/callback",
            Some(pkce.verifier.clone()),
            interaction.cancel(),
        )
        .await
        .ok();
        interaction.notify(AuthEvent::AuthUrl {
            url: self.authorize_url(&pkce.challenge, &pkce.verifier, &self.redirect_uri),
            instructions: Some("Complete login in your browser. If the browser is on another machine, paste the final redirect URL here.".into()),
        });
        let received = callback_or_manual(
            interaction,
            server.as_mut(),
            "Complete login in your browser, or paste the authorization code / redirect URL here:",
            &self.redirect_uri,
        )
        .await?;
        drop(server);
        let (code, state) = match received {
            Received::Callback(code) => (Some(code), pkce.verifier.clone()),
            Received::Manual(input) => {
                let (code, state) = parse_authorization_input(&input);
                if state.as_ref().is_some_and(|state| *state != pkce.verifier) {
                    return Err(AuthError::failed("OAuth state mismatch"));
                }
                (code, state.unwrap_or_else(|| pkce.verifier.clone()))
            }
        };
        let code = code.ok_or_else(|| AuthError::failed("Missing authorization code"))?;
        interaction.notify(AuthEvent::Progress {
            message: EXCHANGING.into(),
        });
        self.exchange(
            &code,
            &state,
            &pkce.verifier,
            &self.redirect_uri,
            interaction.cancel(),
        )
        .await
    }

    async fn login_copy_code(
        &self,
        interaction: &Interaction,
    ) -> Result<OAuthCredential, AuthError> {
        let pkce = pkce::generate();
        interaction.notify(AuthEvent::AuthUrl {
            url: self.authorize_url(&pkce.challenge, &pkce.verifier, &self.copy_code_redirect_uri),
            instructions: Some(
                "Complete login in your browser, then copy the code Anthropic shows and paste it here."
                    .into(),
            ),
        });
        let input = interaction
            .prompt(AuthPrompt::ManualCode {
                message: "Paste the code Anthropic shows after you sign in:".into(),
                placeholder: Some("code#state".into()),
            })
            .await?;
        let (code, state) = parse_authorization_input(&input);
        if state.as_ref().is_some_and(|state| *state != pkce.verifier) {
            return Err(AuthError::failed("OAuth state mismatch"));
        }
        let code = code.ok_or_else(|| AuthError::failed("Missing authorization code"))?;
        interaction.notify(AuthEvent::Progress {
            message: EXCHANGING.into(),
        });
        let state = state.unwrap_or_else(|| pkce.verifier.clone());
        self.exchange(
            &code,
            &state,
            &pkce.verifier,
            &self.copy_code_redirect_uri,
            interaction.cancel(),
        )
        .await
    }

    async fn refresh_token(
        &self,
        refresh: &str,
        cancel: &CancellationToken,
    ) -> Result<OAuthCredential, AuthError> {
        let body = super::object(&[
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("refresh_token", refresh),
        ]);
        let text = self
            .post_json(&body, cancel)
            .await
            .map_err(|err| match err {
                AuthError::Cancelled => AuthError::Cancelled,
                AuthError::Failed(message) => AuthError::Failed(format!(
                    "Anthropic token refresh request failed. url={}; details=Error: {message}",
                    self.token_url
                )),
            })?;
        parse_token(&text).map(credential).map_err(|details| {
            AuthError::Failed(format!(
                "Anthropic token refresh returned invalid JSON. url={}; body={text}; details={details}",
                self.token_url
            ))
        })
    }
}

impl OAuthProvider for AnthropicOAuth {
    fn name(&self) -> &str {
        "Anthropic (Claude Pro/Max)"
    }

    fn login<'a>(
        &'a self,
        interaction: &'a Interaction,
        _options: &'a super::LoginOptions,
    ) -> BoxFuture<'a, Result<OAuthCredential, AuthError>> {
        Box::pin(async move {
            let method = interaction
                .prompt(AuthPrompt::Select {
                    message: "Select Anthropic login method:".into(),
                    options: vec![
                        SelectOption {
                            id: BROWSER.into(),
                            label: "Browser login (default)".into(),
                        },
                        SelectOption {
                            id: COPY_CODE.into(),
                            label: "Copy code login (headless)".into(),
                        },
                    ],
                })
                .await?;
            match method.as_str() {
                COPY_CODE => self.login_copy_code(interaction).await,
                BROWSER => self.login_browser(interaction).await,
                other => Err(AuthError::Failed(format!(
                    "Unknown Anthropic login method: {other}"
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
