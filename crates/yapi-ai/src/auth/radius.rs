//! Radius gateway sign-in: a browser PKCE flow on a fixed loopback port, or
//! an RFC 8628 device authorization, both against the gateway's OAuth API.
//! Port of `oauth/radius.ts`.

use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;
use url::Url;
use yapi_types::auth::OAuthCredential;

use super::callback::{Reply, Server, error_page, success_page};
use super::device::{Poll, poll_device_code};
use super::{
    AuthError, AuthEvent, AuthPrompt, BoxFuture, Interaction, LoginOptions, OAuthProvider,
    SelectOption, form, now_ms, pkce, send,
};

const CALLBACK_HOST: &str = "127.0.0.1";
const CALLBACK_PATH: &str = "/oauth/callback";
const TOKEN_EXPIRY_SKEW_MS: u64 = 60_000;
const CLIENT_ID: &str = "pi-gateway";
const SCOPE: &str = "gateway offline_access";
const DEVICE_CODE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// The default gateway.
pub const DEFAULT_GATEWAY: &str = "https://radius.pi.dev";

/// A gateway URL with a scheme and no trailing slash. Port of
/// `normalizeRadiusGatewayUrl`.
pub fn normalize_gateway(value: &str) -> String {
    let lower = value.to_lowercase();
    let with_scheme = if lower.starts_with("http://") || lower.starts_with("https://") {
        value.to_owned()
    } else {
        format!("https://{value}")
    };
    with_scheme.trim_end_matches('/').to_owned()
}

/// The sign-in of one Radius gateway.
#[derive(Clone, Debug)]
pub struct RadiusOAuth {
    /// Provider display name, shown in prompts.
    pub name: String,
    /// Gateway origin.
    pub gateway: String,
    /// Loopback port of the browser callback.
    pub callback_port: u16,
}

impl RadiusOAuth {
    /// The sign-in for the gateway at `gateway`.
    pub fn new(name: &str, gateway: &str) -> RadiusOAuth {
        RadiusOAuth {
            name: name.to_owned(),
            gateway: normalize_gateway(gateway),
            callback_port: 1456,
        }
    }

    fn redirect_uri(&self) -> String {
        format!(
            "http://{CALLBACK_HOST}:{}{CALLBACK_PATH}",
            self.callback_port
        )
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.gateway)
    }
}

/// pi's `OAuthResponseError`: the message, and the OAuth error code.
struct OAuthFailure {
    message: String,
    code: Option<String>,
}

async fn oauth_failure(response: reqwest::Response, message: &str) -> OAuthFailure {
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    let (code, description) = match serde_json::from_str::<Value>(&text) {
        Ok(data) => (
            data["error"].as_str().map(str::to_owned),
            data["error_description"].as_str().map(str::to_owned),
        ),
        Err(_) if !text.is_empty() => (None, Some(text)),
        Err(_) => (None, None),
    };
    let detail = match (&code, &description) {
        (Some(code), Some(description)) => format!("{code}: {description}"),
        (Some(code), None) => code.clone(),
        (None, Some(description)) if !description.is_empty() => description.clone(),
        _ => status.to_string(),
    };
    OAuthFailure {
        message: format!("{message}: {detail}"),
        code,
    }
}

impl RadiusOAuth {
    /// A token request; an OAuth error keeps its code for device polling.
    async fn token(
        &self,
        fields: &[(&str, &str)],
        cancel: &CancellationToken,
    ) -> Result<Result<OAuthCredential, OAuthFailure>, AuthError> {
        let request = crate::http::client()
            .post(self.url("/v1/oauth/token"))
            .header("accept", "application/json")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(form(fields));
        let response = send(request, cancel).await?;
        if !response.status().is_success() {
            return Ok(Err(oauth_failure(
                response,
                "Radius OAuth token request failed",
            )
            .await));
        }
        let data = super::json_body(response).await;
        let mut extra = Map::new();
        if let Some(scope) = data["scope"].as_str() {
            extra.insert("scope".into(), Value::String(scope.to_owned()));
        }
        let expires_in = data["expires_in"].as_f64().unwrap_or(0.0);
        Ok(Ok(OAuthCredential {
            access: data["access_token"].as_str().unwrap_or_default().to_owned(),
            refresh: data["refresh_token"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            expires: (now_ms() + (expires_in * 1000.0) as u64).saturating_sub(TOKEN_EXPIRY_SKEW_MS),
            extra,
        }))
    }

    async fn login_browser(&self, interaction: &Interaction) -> Result<OAuthCredential, AuthError> {
        let cancel = interaction.cancel();
        let response = send(
            crate::http::client()
                .get(self.url("/v1/oauth"))
                .header("accept", "application/json"),
            cancel,
        )
        .await?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let text = response.text().await.unwrap_or_default();
            return Err(AuthError::Failed(format!(
                "Could not load Radius OAuth config from {}: {status} {text}",
                self.gateway
            )));
        }
        let discovery = super::json_body(response).await;
        let endpoint = discovery["authorizationEndpoint"].as_str().ok_or_else(|| {
            AuthError::Failed(format!("Invalid Radius OAuth config from {}", self.gateway))
        })?;
        let pkce = pkce::generate();
        let state = random_uuid();
        let redirect_uri = self.redirect_uri();
        let mut authorize =
            Url::parse(endpoint).map_err(|err| AuthError::Failed(err.to_string()))?;
        authorize.set_query(Some(&form(&[
            ("response_type", "code"),
            ("client_id", CLIENT_ID),
            ("redirect_uri", &redirect_uri),
            ("scope", SCOPE),
            ("code_challenge", &pkce.challenge),
            ("code_challenge_method", "S256"),
            ("handoff", "url"),
            ("state", &state),
        ])));
        let expected_state = state.clone();
        let handler = move |method: &str, url: &Url| -> Reply<String> {
            if method != "GET" || url.path() != CALLBACK_PATH {
                return Reply::page(404, error_page("Callback route not found.", None));
            }
            let query = |name: &str| {
                url.query_pairs()
                    .find(|(key, _)| key == name)
                    .map(|(_, value)| value.into_owned())
            };
            if query("state").as_deref() != Some(expected_state.as_str()) {
                return Reply::page(400, error_page("State mismatch.", None));
            }
            if let Some(error) = query("error") {
                let description = query("error_description").unwrap_or(error);
                return Reply {
                    status: 400,
                    html: error_page("Radius authorization failed.", Some(&description)),
                    outcome: Some(Err(AuthError::Failed(format!(
                        "Radius authorization failed: {description}"
                    )))),
                };
            }
            match query("code").filter(|code| !code.is_empty()) {
                Some(code) => Reply {
                    status: 200,
                    html: success_page("Signed in to Radius. You may now close this page."),
                    outcome: Some(Ok(code)),
                },
                None => Reply::page(400, error_page("Missing authorization code.", None)),
            }
        };
        let mut server = Server::start(
            CALLBACK_HOST,
            self.callback_port,
            CALLBACK_PATH,
            cancel,
            Box::new(handler),
        )
        .await
        .map_err(|err| AuthError::Failed(err.to_string()))?;
        interaction.notify(AuthEvent::Progress {
            message: format!("Listening for OAuth callback on {redirect_uri}"),
        });
        interaction.notify(AuthEvent::AuthUrl {
            url: authorize.to_string(),
            instructions: Some("Continue in your browser.".into()),
        });
        let code = server.wait().await?;
        drop(server);
        self.token(
            &[
                ("grant_type", "authorization_code"),
                ("client_id", CLIENT_ID),
                ("redirect_uri", &redirect_uri),
                ("code", &code),
                ("code_verifier", &pkce.verifier),
            ],
            cancel,
        )
        .await?
        .map_err(|failure| AuthError::Failed(failure.message))
    }

    async fn login_device(&self, interaction: &Interaction) -> Result<OAuthCredential, AuthError> {
        let cancel = interaction.cancel();
        let request = crate::http::client()
            .post(self.url("/v1/oauth/device"))
            .header("accept", "application/json")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(form(&[("client_id", CLIENT_ID), ("scope", SCOPE)]));
        let response = send(request, cancel).await?;
        if !response.status().is_success() {
            let failure = oauth_failure(response, "Radius OAuth device authorization failed").await;
            return Err(AuthError::Failed(failure.message));
        }
        let data = super::json_body(response).await;
        let (Some(device_code), Some(user_code), Some(verification_uri), Some(expires_in)) = (
            data["device_code"].as_str().filter(|v| !v.is_empty()),
            data["user_code"].as_str().filter(|v| !v.is_empty()),
            data["verification_uri"].as_str().filter(|v| !v.is_empty()),
            data["expires_in"].as_f64().filter(|v| *v != 0.0),
        ) else {
            return Err(AuthError::failed(
                "Radius OAuth device authorization response is missing required fields",
            ));
        };
        let interval = data["interval"].as_f64();
        interaction.notify(AuthEvent::DeviceCode {
            user_code: user_code.to_owned(),
            verification_uri: verification_uri.to_owned(),
            interval_seconds: interval,
            expires_in_seconds: Some(expires_in),
        });
        poll_device_code(interval, Some(expires_in), false, cancel, || async {
            match self
                .token(
                    &[
                        ("grant_type", DEVICE_CODE_GRANT),
                        ("client_id", CLIENT_ID),
                        ("device_code", device_code),
                    ],
                    cancel,
                )
                .await?
            {
                Ok(credential) => Ok(Poll::Complete(credential)),
                Err(failure) => match failure.code.as_deref() {
                    Some("authorization_pending") => Ok(Poll::Pending),
                    Some("slow_down") => Ok(Poll::SlowDown(None)),
                    Some("expired_token") => {
                        Ok(Poll::Failed("Device authorization expired.".into()))
                    }
                    Some("access_denied") => {
                        Ok(Poll::Failed("Device authorization was denied.".into()))
                    }
                    _ => Err(AuthError::Failed(failure.message)),
                },
            }
        })
        .await
    }

    async fn login_radius(&self, interaction: &Interaction) -> Result<OAuthCredential, AuthError> {
        let method = interaction
            .prompt(AuthPrompt::Select {
                message: format!("Sign in to {}:", self.name),
                options: vec![
                    SelectOption {
                        id: "browser".into(),
                        label: "Sign in with browser (recommended)".into(),
                    },
                    SelectOption {
                        id: "device-code".into(),
                        label: "Sign in with device code (when signing in from another device)"
                            .into(),
                    },
                ],
            })
            .await?;
        match method.as_str() {
            "device-code" => self.login_device(interaction).await,
            "browser" => self.login_browser(interaction).await,
            other => Err(AuthError::Failed(format!(
                "Unknown {} sign-in method: {other}",
                self.name
            ))),
        }
    }
}

fn random_uuid() -> String {
    let mut bytes = [0u8; 16];
    let _ = getrandom::fill(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

impl OAuthProvider for RadiusOAuth {
    fn name(&self) -> &str {
        &self.name
    }

    fn is_subscription(&self) -> bool {
        false
    }

    fn login<'a>(
        &'a self,
        interaction: &'a Interaction,
        _options: &'a LoginOptions,
    ) -> BoxFuture<'a, Result<OAuthCredential, AuthError>> {
        Box::pin(self.login_radius(interaction))
    }

    fn refresh<'a>(
        &'a self,
        credential: &'a OAuthCredential,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<OAuthCredential, AuthError>> {
        Box::pin(async move {
            self.token(
                &[
                    ("grant_type", "refresh_token"),
                    ("client_id", CLIENT_ID),
                    ("refresh_token", &credential.refresh),
                ],
                cancel,
            )
            .await?
            .map_err(|failure| AuthError::Failed(failure.message))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_gateways() {
        assert_eq!(
            normalize_gateway("radius.example.com/"),
            "https://radius.example.com"
        );
        assert_eq!(
            normalize_gateway("HTTP://127.0.0.1:9//"),
            "HTTP://127.0.0.1:9"
        );
    }
}
