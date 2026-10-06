//! Sign in with ChatGPT for the `openai` provider: a public client registered
//! per sign-in, whose user token goes straight to api.openai.com. Port of
//! `oauth/openai-chatgpt.ts`.

use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;
use url::Url;
use yapi_types::auth::OAuthCredential;

use super::anthropic::expiry;
use super::callback::{Reply, Server, error_page, query, success_page};
use super::{
    AuthError, AuthEvent, AuthPrompt, BoxFuture, Interaction, LoginOptions, OAuthProvider,
    callback_host, error_text, form, json_body, pkce, post_form, send,
};

/// Every sign-in registers a new client under this id; the callback carries the issued one.
const DYNAMIC_CLIENT_ID: &str = "dynamic_agent_client";
const AGENT_NAME_HINT: &str = "yapi";
const RESOURCE: &str = "https://api.openai.com/v1";
const CALLBACK_PATH: &str = "/auth/callback";
const DIRECT_TOKEN_SCOPE: &str = "chatgpt.tokens.use.direct";
const SCOPE: &str = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
/// Refresh this long before the real expiry.
const EXPIRY_MARGIN_MS: f64 = 3.0 * 60.0 * 1000.0;

/// The ChatGPT sign-in and its endpoints.
#[derive(Clone, Debug)]
pub struct ChatGptOAuth {
    /// Authorization page.
    pub authorize_url: String,
    /// Token endpoint.
    pub token_url: String,
    /// Loopback callback port.
    pub callback_port: u16,
    /// Redirect URI; must reach the callback port.
    pub redirect_uri: String,
}

impl Default for ChatGptOAuth {
    fn default() -> ChatGptOAuth {
        ChatGptOAuth {
            authorize_url: "https://auth.openai.com/api/accounts/authorize".into(),
            token_url: "https://auth.openai.com/api/accounts/oauth/token".into(),
            callback_port: 1455,
            redirect_uri: "http://127.0.0.1:1455/auth/callback".into(),
        }
    }
}

/// The code and the client id the authorization server issued.
#[derive(Debug)]
struct Authorization {
    code: String,
    client_id: String,
}

fn from_callback(url: &Url, expected_state: &str) -> Result<Authorization, AuthError> {
    let code = query(url, "code").filter(|code| !code.is_empty());
    let code = code.ok_or_else(|| AuthError::failed("Missing authorization code"))?;
    let state = query(url, "state").filter(|state| !state.is_empty());
    let state = state.ok_or_else(|| AuthError::failed("Missing OAuth state"))?;
    if state != expected_state {
        return Err(AuthError::failed("OAuth state mismatch"));
    }
    let client_id = query(url, "client_id")
        .map(|id| id.trim().to_owned())
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            AuthError::failed(
                "OpenAI OAuth registration callback did not contain an issued client ID",
            )
        })?;
    Ok(Authorization { code, client_id })
}

fn is_uuid(value: &str) -> bool {
    let groups: Vec<&str> = value.split('-').collect();
    groups.len() == 5
        && groups.iter().zip([8, 4, 4, 4, 12]).all(|(group, length)| {
            group.len() == length && group.chars().all(|c| c.is_ascii_hexdigit())
        })
}

fn token_string(token: &Value, field: &str) -> Result<String, AuthError> {
    token
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            AuthError::Failed(format!("OpenAI OAuth token response has invalid {field}"))
        })
}

fn credential(token: &Value, client_id: &str) -> Result<OAuthCredential, AuthError> {
    let access = token_string(token, "access_token")?;
    let refresh = token_string(token, "refresh_token")?;
    let scope = token_string(token, "scope")?;
    let expires_in = token
        .get("expires_in")
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value > 0.0)
        .ok_or_else(|| AuthError::failed("OpenAI OAuth token response has invalid expires_in"))?;
    let scopes: Vec<Value> = scope
        .split_whitespace()
        .map(|scope| Value::String(scope.to_owned()))
        .collect();
    if !scopes.iter().any(|scope| scope == DIRECT_TOKEN_SCOPE) {
        return Err(AuthError::Failed(format!(
            "OpenAI OAuth grant did not include {DIRECT_TOKEN_SCOPE}"
        )));
    }
    let mut extra = Map::new();
    extra.insert("clientId".into(), Value::String(client_id.to_owned()));
    extra.insert("scopes".into(), Value::Array(scopes));
    Ok(OAuthCredential {
        access,
        refresh,
        expires: expiry(expires_in, EXPIRY_MARGIN_MS),
        extra,
    })
}

impl ChatGptOAuth {
    async fn request_token(
        &self,
        fields: &[(&str, &str)],
        cancel: &CancellationToken,
    ) -> Result<Value, AuthError> {
        let request = post_form(&self.token_url, fields);
        let response = send(request, cancel).await?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = error_text(response).await;
            return Err(AuthError::Failed(format!(
                "OpenAI OAuth token request failed ({status}): {text}"
            )));
        }
        match json_body(response).await {
            object @ Value::Object(_) => Ok(object),
            _ => Err(AuthError::failed(
                "OpenAI OAuth token response must be an object",
            )),
        }
    }

    fn parse_manual_input(&self, input: &str, state: &str) -> Result<Authorization, AuthError> {
        let url = Url::parse(input.trim())
            .map_err(|_| AuthError::failed("Paste the full callback URL from the browser"))?;
        let expected =
            Url::parse(&self.redirect_uri).map_err(|err| AuthError::Failed(err.to_string()))?;
        if url.origin() != expected.origin() || url.path() != expected.path() {
            return Err(AuthError::Failed(format!(
                "The pasted callback URL must start with {}",
                self.redirect_uri
            )));
        }
        if let Some(error) = query(&url, "error") {
            return Err(AuthError::Failed(format!(
                "ChatGPT authorization failed: {error}"
            )));
        }
        from_callback(&url, state)
    }

    async fn start_server(
        &self,
        state: String,
        cancel: &CancellationToken,
    ) -> std::io::Result<Server<Authorization>> {
        let handler = move |_method: &str, url: &Url| -> Reply<Authorization> {
            if url.path() != CALLBACK_PATH {
                return Reply::page(404, error_page("Callback route not found.", None));
            }
            if let Some(error) = query(url, "error") {
                return Reply {
                    status: 400,
                    html: error_page(
                        "ChatGPT was not connected.",
                        Some(&format!("Error: {error}")),
                    ),
                    outcome: Some(Err(AuthError::Failed(format!(
                        "ChatGPT authorization failed: {error}"
                    )))),
                };
            }
            match from_callback(url, &state) {
                Ok(authorization) => Reply {
                    status: 200,
                    html: success_page(
                        "ChatGPT authentication completed. You can close this window.",
                    ),
                    outcome: Some(Ok(authorization)),
                },
                Err(err) => Reply::page(400, error_page(&err.to_string(), None)),
            }
        };
        Server::start(
            &callback_host(),
            self.callback_port,
            CALLBACK_PATH,
            cancel,
            Box::new(handler),
        )
        .await
    }

    async fn login_chatgpt(
        &self,
        interaction: &Interaction,
        options: &LoginOptions,
    ) -> Result<OAuthCredential, AuthError> {
        let device_id = options
            .device_id
            .as_deref()
            .filter(|id| is_uuid(id))
            .ok_or_else(|| {
                AuthError::failed(
                    "Sign in with ChatGPT requires a device ID (UUID) for this installation",
                )
            })?;
        let host_id = format!("urn:uuid:{}", device_id.to_ascii_lowercase());
        let pkce = pkce::generate();
        let state = pkce::random_base64url();
        let nonce = pkce::random_base64url();
        let mut server = match self.start_server(state.clone(), interaction.cancel()).await {
            Ok(server) => Some(server),
            Err(err) => {
                interaction.notify(AuthEvent::Info {
                    message: format!(
                        "Could not listen on {}; paste the final redirect URL to continue. {err}",
                        self.redirect_uri
                    ),
                    links: Vec::new(),
                });
                None
            }
        };
        let query = form(&[
            ("client_id", DYNAMIC_CLIENT_ID),
            ("agent_name_hint", AGENT_NAME_HINT),
            ("ext_agent_host_id", &host_id),
            ("response_type", "code"),
            ("redirect_uri", &self.redirect_uri),
            ("resource", RESOURCE),
            ("scope", SCOPE),
            ("state", &state),
            ("code_challenge", &pkce.challenge),
            ("code_challenge_method", "S256"),
            ("nonce", &nonce),
        ]);
        interaction.notify(AuthEvent::AuthUrl {
            url: format!("{}?{query}", self.authorize_url),
            instructions: Some("Complete sign-in in your browser. If the callback does not complete, paste the final redirect URL here.".into()),
        });
        let withdraw = interaction.cancel().child_token();
        let _withdraw_on_return = withdraw.clone().drop_guard();
        let manual = async {
            let input = interaction
                .prompt_until(
                    AuthPrompt::ManualCode {
                        message:
                            "Complete login in your browser, or paste the final redirect URL here:"
                                .into(),
                        placeholder: Some(self.redirect_uri.clone()),
                    },
                    withdraw,
                )
                .await?;
            self.parse_manual_input(&input, &state)
        };
        let authorization = match server.as_mut() {
            Some(server) => tokio::select! {
                result = server.wait() => result,
                result = manual => result,
            },
            None => manual.await,
        }?;
        drop(server);
        interaction.notify(AuthEvent::Progress {
            message: "Exchanging authorization code for tokens...".into(),
        });
        let token = self
            .request_token(
                &[
                    ("grant_type", "authorization_code"),
                    ("client_id", &authorization.client_id),
                    ("code", &authorization.code),
                    ("code_verifier", &pkce.verifier),
                    ("redirect_uri", &self.redirect_uri),
                    ("resource", RESOURCE),
                ],
                interaction.cancel(),
            )
            .await?;
        // The ID token is part of the response contract; yapi does not read it.
        if token
            .get("id_token")
            .and_then(Value::as_str)
            .is_none_or(|id| id.trim().is_empty())
        {
            return Err(AuthError::failed(
                "OpenAI OAuth token response did not contain an ID token",
            ));
        }
        credential(&token, &authorization.client_id)
    }
}

impl OAuthProvider for ChatGptOAuth {
    fn name(&self) -> &str {
        "OpenAI (ChatGPT subscription)"
    }

    fn login_label(&self) -> Option<&str> {
        Some("Sign in with ChatGPT")
    }

    fn login<'a>(
        &'a self,
        interaction: &'a Interaction,
        options: &'a LoginOptions,
    ) -> BoxFuture<'a, Result<OAuthCredential, AuthError>> {
        Box::pin(async move {
            self.login_chatgpt(interaction, options)
                .await
                .map_err(|err| {
                    if interaction.cancel().is_cancelled() {
                        AuthError::Cancelled
                    } else {
                        err
                    }
                })
        })
    }

    fn refresh<'a>(
        &'a self,
        credential: &'a OAuthCredential,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<OAuthCredential, AuthError>> {
        Box::pin(async move {
            let client_id = credential
                .extra
                .get("clientId")
                .and_then(Value::as_str)
                .filter(|id| !id.trim().is_empty())
                .ok_or_else(|| {
                    AuthError::failed(
                        "Stored OpenAI OAuth credential does not contain an issued client ID; reconnect ChatGPT",
                    )
                })?;
            let token = self
                .request_token(
                    &[
                        ("grant_type", "refresh_token"),
                        ("client_id", client_id),
                        ("refresh_token", &credential.refresh),
                        ("resource", RESOURCE),
                    ],
                    cancel,
                )
                .await?;
            super::chatgpt::credential(&token, client_id)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_device_ids_and_callbacks() {
        assert!(is_uuid("123e4567-E89b-12d3-a456-426614174000"));
        assert!(!is_uuid("123e4567e89b12d3a456426614174000"));
        let oauth = ChatGptOAuth::default();
        let error = |input: &str| {
            oauth
                .parse_manual_input(input, "s")
                .unwrap_err()
                .to_string()
        };
        assert_eq!(
            error("nope"),
            "Paste the full callback URL from the browser"
        );
        assert_eq!(
            error("http://localhost:1455/auth/callback?code=c"),
            "The pasted callback URL must start with http://127.0.0.1:1455/auth/callback"
        );
        assert_eq!(
            error("http://127.0.0.1:1455/auth/callback?code=c&state=x&client_id=i"),
            "OAuth state mismatch"
        );
        let ok = oauth
            .parse_manual_input(
                "http://127.0.0.1:1455/auth/callback?code=c&state=s&client_id=i",
                "s",
            )
            .unwrap();
        assert_eq!((ok.code.as_str(), ok.client_id.as_str()), ("c", "i"));
    }
}
