//! OpenRouter OAuth: a PKCE sign-in that trades the authorization code for a
//! permanent, user-controlled API key. Port of `oauth/openrouter.ts`.

use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;
use url::Url;
use yapi_types::auth::OAuthCredential;

use super::callback::{Reply, Server, error_page, query, success_page};
use super::{
    AuthError, AuthEvent, AuthPrompt, BoxFuture, Interaction, LoginOptions, OAuthProvider,
    callback_host, form, pkce, send,
};

const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const TOKEN_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);
/// pi's credentials for OpenRouter never expire: `Number.MAX_SAFE_INTEGER`.
const NEVER: u64 = 9_007_199_254_740_991;

/// The OpenRouter sign-in and its endpoints.
#[derive(Clone, Debug)]
pub struct OpenRouterOAuth {
    /// Authorization page.
    pub authorize_url: String,
    /// Key exchange endpoint.
    pub token_url: String,
}

impl Default for OpenRouterOAuth {
    fn default() -> OpenRouterOAuth {
        OpenRouterOAuth {
            authorize_url: "https://openrouter.ai/auth".into(),
            token_url: "https://openrouter.ai/api/v1/auth/keys".into(),
        }
    }
}

/// The code in pasted input: a redirect URL, a `code=` query, or the code.
fn parse_input(input: &str) -> Option<String> {
    let value = input.trim();
    if value.is_empty() {
        return None;
    }
    if let Ok(url) = Url::parse(value) {
        return query(&url, "code");
    }
    if value.contains("code=") {
        return url::form_urlencoded::parse(value.as_bytes())
            .find(|(key, _)| key == "code")
            .map(|(_, code)| code.into_owned());
    }
    Some(value.to_owned())
}

fn error_detail(body: &Value) -> Option<String> {
    ["error_description", "message", "error"]
        .iter()
        .find_map(|key| body[*key].as_str().map(str::to_owned))
        .or_else(|| body["error"]["message"].as_str().map(str::to_owned))
}

impl OpenRouterOAuth {
    async fn exchange(
        &self,
        code: &str,
        verifier: &str,
        cancel: &CancellationToken,
    ) -> Result<OAuthCredential, AuthError> {
        let request = crate::http::client()
            .post(&self.token_url)
            .header("accept", "application/json")
            .header("content-type", "application/json")
            .body(
                yapi_types::json::to_string(&json!({
                    "code": code,
                    "code_verifier": verifier,
                    "code_challenge_method": "S256",
                }))
                .unwrap_or_default(),
            );
        let response =
            match tokio::time::timeout(TOKEN_EXCHANGE_TIMEOUT, send(request, cancel)).await {
                Ok(response) => response?,
                Err(_) => {
                    return Err(AuthError::failed(
                        "OpenRouter OAuth token exchange timed out",
                    ));
                }
            };
        let status = response.status();
        let bytes = response.bytes().await.unwrap_or_default();
        let body = match serde_json::from_slice::<Value>(&bytes) {
            Ok(object @ Value::Object(_)) => object,
            Ok(_) => Value::Object(Map::new()),
            Err(_) if status.is_success() => {
                return Err(AuthError::failed("OpenRouter OAuth returned invalid JSON"));
            }
            Err(_) => Value::Object(Map::new()),
        };
        if !status.is_success() {
            let detail = error_detail(&body)
                .map(|detail| format!(": {detail}"))
                .unwrap_or_default();
            return Err(AuthError::Failed(format!(
                "OpenRouter OAuth key exchange failed (HTTP {}){detail}",
                status.as_u16()
            )));
        }
        let key = body["key"]
            .as_str()
            .filter(|key| !key.is_empty())
            .ok_or_else(|| AuthError::failed("OpenRouter OAuth response carries no \"key\""))?;
        Ok(OAuthCredential {
            access: key.to_owned(),
            refresh: String::new(),
            expires: NEVER,
            extra: Map::new(),
        })
    }

    async fn login_openrouter(
        &self,
        interaction: &Interaction,
    ) -> Result<OAuthCredential, AuthError> {
        let pkce = pkce::generate();
        // OpenRouter sends no `state`; the random path keeps stray requests
        // from completing the sign-in.
        let path = format!("/oauth/callback/{}", yapi_types::time::uuid_v4());
        let route = path.clone();
        let handler = move |method: &str, url: &Url| -> Reply<String> {
            if method != "GET" || url.path() != route {
                return Reply::page(404, error_page("Callback route not found.", None));
            }
            if let Some(error) = query(url, "error") {
                let description = query(url, "error_description").unwrap_or(error);
                return Reply {
                    status: 400,
                    html: error_page("OpenRouter authorization failed.", Some(&description)),
                    outcome: Some(Err(AuthError::Failed(format!(
                        "OpenRouter authorization failed: {description}"
                    )))),
                };
            }
            match query(url, "code").filter(|code| !code.is_empty()) {
                Some(code) => Reply {
                    status: 200,
                    html: success_page("Signed in to OpenRouter. You may now close this page."),
                    outcome: Some(Ok(code)),
                },
                None => Reply::page(400, error_page("Missing authorization code.", None)),
            }
        };
        let mut server = Server::start(
            &callback_host(),
            0,
            &path,
            interaction.cancel(),
            Box::new(handler),
        )
        .await
        .map_err(|err| AuthError::Failed(err.to_string()))?;
        let redirect_uri = server.redirect_uri().to_owned();
        let query = form(&[
            ("callback_url", &redirect_uri),
            ("code_challenge", &pkce.challenge),
            ("code_challenge_method", "S256"),
        ]);
        interaction.notify(AuthEvent::Progress {
            message: format!("Listening for OpenRouter OAuth callback on {redirect_uri}"),
        });
        interaction.notify(AuthEvent::AuthUrl {
            url: format!("{}?{query}", self.authorize_url),
            instructions: Some("Complete sign-in in your browser. If the browser is on another machine, paste the final redirect URL here.".into()),
        });
        let withdraw = interaction.cancel().child_token();
        let _withdraw_on_return = withdraw.clone().drop_guard();
        let manual = async {
            let input = interaction
                .prompt_until(
                    AuthPrompt::ManualCode {
                        message: "Complete sign-in in your browser, or paste the authorization code / redirect URL here:".into(),
                        placeholder: Some(redirect_uri.clone()),
                    },
                    withdraw,
                )
                .await?;
            let code = parse_input(&input)
                .ok_or_else(|| AuthError::failed("Missing authorization code"))?;
            interaction.notify(AuthEvent::Progress {
                message: "Exchanging authorization code for an API key...".into(),
            });
            Ok::<String, AuthError>(code)
        };
        let race = async {
            tokio::select! {
                result = server.wait() => result,
                result = manual => result,
            }
        };
        let code = tokio::time::timeout(LOGIN_TIMEOUT, race)
            .await
            .unwrap_or_else(|_| Err(AuthError::failed("OpenRouter sign-in timed out")))?;
        drop(server);
        self.exchange(&code, &pkce.verifier, interaction.cancel())
            .await
    }
}

impl OAuthProvider for OpenRouterOAuth {
    fn name(&self) -> &str {
        "OpenRouter OAuth"
    }

    fn login_label(&self) -> Option<&str> {
        Some("Sign in with OpenRouter")
    }

    fn is_subscription(&self) -> bool {
        false
    }

    fn login<'a>(
        &'a self,
        interaction: &'a Interaction,
        _options: &'a LoginOptions,
    ) -> BoxFuture<'a, Result<OAuthCredential, AuthError>> {
        Box::pin(self.login_openrouter(interaction))
    }

    fn refresh<'a>(
        &'a self,
        credential: &'a OAuthCredential,
        _cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<OAuthCredential, AuthError>> {
        Box::pin(async move { Ok(credential.clone()) })
    }
}
