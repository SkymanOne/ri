//! Kimi Code (subscription): an RFC 8628 device authorization against
//! auth.kimi.com, whose access token authenticates as a bearer header. Port
//! of `oauth/kimi-coding.ts`.

use std::time::Duration;

use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;
use yapi_types::auth::OAuthCredential;

use super::device::{DEVICE_CODE_GRANT, Poll, poll_device_code, positive, trusted_url};
use super::{
    AuthError, AuthEvent, BoxFuture, Interaction, LoginOptions, OAuthAuth, OAuthProvider,
    json_body, now_ms, post_form, send,
};

const CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
const DEFAULT_OAUTH_HOST: &str = "https://auth.kimi.com";
const DEVICE_CODE_TIMEOUT_SECONDS: f64 = 15.0 * 60.0;
const DEFAULT_POLL_INTERVAL_SECONDS: f64 = 5.0;
const REFRESH_MAX_RETRIES: u32 = 3;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The Kimi Code sign-in.
#[derive(Clone, Debug, Default)]
pub struct KimiOAuth {
    /// Overrides the OAuth host, as `KIMI_CODE_OAUTH_HOST` does.
    pub oauth_host: Option<String>,
}

impl KimiOAuth {
    /// `KIMI_CODE_OAUTH_HOST`, else `KIMI_OAUTH_HOST`, else auth.kimi.com.
    fn host(&self) -> String {
        self.oauth_host
            .clone()
            .or_else(|| crate::credentials::provider_env_value("KIMI_CODE_OAUTH_HOST", None))
            .or_else(|| crate::credentials::provider_env_value("KIMI_OAUTH_HOST", None))
            .unwrap_or_else(|| DEFAULT_OAUTH_HOST.to_owned())
            .trim_end_matches('/')
            .to_owned()
    }
}

/// A credential from a token response, or pi's error for `operation`.
fn token(json: &Value, operation: &str) -> Result<OAuthCredential, AuthError> {
    let access = json["access_token"]
        .as_str()
        .filter(|value| !value.is_empty());
    let refresh = json["refresh_token"]
        .as_str()
        .filter(|value| !value.is_empty());
    let expires_in = positive(&json["expires_in"]);
    match (access, refresh, expires_in) {
        (Some(access), Some(refresh), Some(expires_in)) => Ok(OAuthCredential {
            access: access.to_owned(),
            refresh: refresh.to_owned(),
            expires: now_ms() + (expires_in * 1000.0) as u64,
            extra: Map::new(),
        }),
        _ => Err(AuthError::Failed(format!(
            "Kimi Code token {operation} response missing fields: {}",
            yapi_types::json::stringify(json)
        ))),
    }
}

impl KimiOAuth {
    async fn login_kimi(&self, interaction: &Interaction) -> Result<OAuthCredential, AuthError> {
        let host = self.host();
        let cancel = interaction.cancel();
        let response = send(
            post_form(
                format!("{host}/api/oauth/device_authorization"),
                &[("client_id", CLIENT_ID)],
            )
            .timeout(REQUEST_TIMEOUT),
            cancel,
        )
        .await?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            let detail = if text.is_empty() {
                String::new()
            } else {
                format!(": {text}")
            };
            return Err(AuthError::Failed(format!(
                "Kimi Code device authorization failed with status {}{detail}",
                status.as_u16()
            )));
        }
        let json = json_body(response).await;
        let (Some(device_code), Some(user_code), Some(_), Some(complete)) = (
            json["device_code"].as_str(),
            json["user_code"].as_str(),
            trusted_url(&json["verification_uri"]),
            trusted_url(&json["verification_uri_complete"]),
        ) else {
            return Err(AuthError::Failed(format!(
                "Invalid Kimi Code device authorization response: {}",
                yapi_types::json::stringify(&json)
            )));
        };
        let interval = positive(&json["interval"]).unwrap_or(DEFAULT_POLL_INTERVAL_SECONDS);
        let expires_in = positive(&json["expires_in"]).unwrap_or(DEVICE_CODE_TIMEOUT_SECONDS);
        // pi opens the complete URI exactly as the server wrote it.
        let complete = json["verification_uri_complete"]
            .as_str()
            .map_or(complete, str::to_owned);
        interaction.notify(AuthEvent::DeviceCode {
            user_code: user_code.to_owned(),
            verification_uri: complete,
            interval_seconds: Some(interval),
            expires_in_seconds: Some(expires_in),
        });
        let token_url = format!("{host}/api/oauth/token");
        poll_device_code(Some(interval), Some(expires_in), true, cancel, || {
            let request = post_form(
                &token_url,
                &[
                    ("client_id", CLIENT_ID),
                    ("device_code", device_code),
                    ("grant_type", DEVICE_CODE_GRANT),
                ],
            )
            .timeout(REQUEST_TIMEOUT);
            async move {
                let response = send(request, cancel).await?;
                let status = response.status().as_u16();
                if status >= 500 {
                    let text = response.text().await.unwrap_or_default();
                    let detail = if text.is_empty() { String::new() } else { format!(": {text}") };
                    return Ok(Poll::Failed(format!(
                        "Kimi Code device token request failed with status {status}{detail}"
                    )));
                }
                let json = json_body(response).await;
                if (200..300).contains(&status) && json["access_token"].is_string() {
                    return Ok(match token(&json, "poll") {
                        Ok(credential) => Poll::Complete(credential),
                        Err(err) => Poll::Failed(err.to_string()),
                    });
                }
                let description = json["error_description"]
                    .as_str()
                    .map(|text| format!(": {text}"))
                    .unwrap_or_default();
                Ok(match json["error"].as_str() {
                    Some("authorization_pending") => Poll::Pending,
                    Some("slow_down") => Poll::SlowDown(positive(&json["interval"])),
                    Some("expired_token") => Poll::Failed(
                        "Kimi Code device authorization expired. Please restart login.".into(),
                    ),
                    Some("access_denied") => Poll::Failed("Kimi Code login was denied.".into()),
                    Some(error) => Poll::Failed(format!(
                        "Kimi Code device token request failed (status {status}): {error}{description}"
                    )),
                    None => Poll::Failed(format!(
                        "Kimi Code device token request failed (status {status})"
                    )),
                })
            }
        })
        .await
    }

    async fn refresh_kimi(
        &self,
        refresh: &str,
        cancel: &CancellationToken,
    ) -> Result<OAuthCredential, AuthError> {
        let url = format!("{}/api/oauth/token", self.host());
        let mut last = None;
        for attempt in 0..=REFRESH_MAX_RETRIES {
            if attempt > 0 {
                let delay = Duration::from_millis(1000 << (attempt - 1));
                cancel
                    .run_until_cancelled(tokio::time::sleep(delay))
                    .await
                    .ok_or(AuthError::Cancelled)?;
            }
            if cancel.is_cancelled() {
                return Err(AuthError::failed("Kimi Code token refresh aborted"));
            }
            let request = post_form(
                &url,
                &[
                    ("client_id", CLIENT_ID),
                    ("grant_type", "refresh_token"),
                    ("refresh_token", refresh),
                ],
            )
            .timeout(REQUEST_TIMEOUT);
            let response = match send(request, cancel).await {
                Ok(response) => response,
                Err(AuthError::Cancelled) => return Err(AuthError::Cancelled),
                Err(err) => {
                    last = Some(err);
                    continue;
                }
            };
            let status = response.status().as_u16();
            let json = json_body(response).await;
            if (200..300).contains(&status) {
                return token(&json, "refresh");
            }
            if status == 401 || status == 403 || json["error"] == "invalid_grant" {
                let description = json["error_description"]
                    .as_str()
                    .map(|text| format!(": {text}"))
                    .unwrap_or_default();
                return Err(AuthError::Failed(format!(
                    "Kimi Code token refresh unauthorized (status {status}){description}"
                )));
            }
            if (status == 429 || status >= 500) && attempt < REFRESH_MAX_RETRIES {
                last = Some(AuthError::Failed(format!(
                    "Kimi Code token refresh failed with status {status}"
                )));
                continue;
            }
            return Err(AuthError::Failed(format!(
                "Kimi Code token refresh failed with status {status}: {}",
                yapi_types::json::stringify(&json)
            )));
        }
        Err(last.unwrap_or_else(|| AuthError::failed("Kimi Code token refresh failed")))
    }
}

impl OAuthProvider for KimiOAuth {
    fn name(&self) -> &str {
        "Kimi Code (subscription)"
    }

    fn login_label(&self) -> Option<&str> {
        Some("Sign in with Kimi Code")
    }

    fn login<'a>(
        &'a self,
        interaction: &'a Interaction,
        _options: &'a LoginOptions,
    ) -> BoxFuture<'a, Result<OAuthCredential, AuthError>> {
        Box::pin(self.login_kimi(interaction))
    }

    fn refresh<'a>(
        &'a self,
        credential: &'a OAuthCredential,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<OAuthCredential, AuthError>> {
        Box::pin(self.refresh_kimi(&credential.refresh, cancel))
    }

    fn to_auth<'a>(
        &'a self,
        credential: &'a OAuthCredential,
    ) -> BoxFuture<'a, Result<OAuthAuth, AuthError>> {
        let mut auth = OAuthAuth::default();
        auth.headers.insert(
            "Authorization".into(),
            Some(format!("Bearer {}", credential.access)),
        );
        Box::pin(async move { Ok(auth) })
    }
}
