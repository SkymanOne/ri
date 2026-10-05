//! Meta (Muse subscription): an RFC 8628 device authorization against
//! auth.meta.com whose identity token mints a Model API key that lives about
//! a day. The identity token is stored as the refresh token and the key as
//! the access token, so a refresh mints a new key. Port of `oauth/meta.ts`.

use std::time::Duration;

use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;
use yapi_types::auth::OAuthCredential;

use super::device::{Poll, poll_device_code};
use super::{
    AuthError, AuthEvent, BoxFuture, Interaction, LoginOptions, OAuthProvider, form, json_body,
    now_ms, send,
};

const CLIENT_ID: &str = "1031625952748946";
const API_KEY_LIFETIME_MS: u64 = 24 * 60 * 60 * 1000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const DEVICE_CODE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// The Meta sign-in and its endpoints.
#[derive(Clone, Debug)]
pub struct MetaOAuth {
    /// Device authorization endpoint.
    pub device_authorization_url: String,
    /// Device token endpoint.
    pub device_token_url: String,
    /// Model API key mint endpoint.
    pub api_key_mint_url: String,
}

impl Default for MetaOAuth {
    fn default() -> MetaOAuth {
        MetaOAuth {
            device_authorization_url: "https://auth.meta.com/oidc/device/authorization/".into(),
            device_token_url: "https://auth.meta.com/oidc/device/token/".into(),
            api_key_mint_url: "https://api.meta.ai/muse-code/key".into(),
        }
    }
}

fn error_detail(json: &Value) -> String {
    ["error_description", "detail", "message", "error"]
        .iter()
        .find_map(|key| {
            json[*key]
                .as_str()
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
        .map(|value| format!(": {value}"))
        .unwrap_or_default()
}

fn trusted_url(value: &Value) -> Option<String> {
    let url = url::Url::parse(value.as_str()?).ok()?;
    matches!(url.scheme(), "http" | "https").then(|| url.to_string())
}

fn positive(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .filter(|value| value.is_finite() && *value > 0.0)
}

fn post_form(url: &str, fields: &[(&str, &str)]) -> reqwest::RequestBuilder {
    crate::http::client()
        .post(url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .timeout(REQUEST_TIMEOUT)
        .body(form(fields))
}

impl MetaOAuth {
    /// A Model API key for an identity token.
    async fn mint(
        &self,
        identity: &str,
        cancel: &CancellationToken,
    ) -> Result<OAuthCredential, AuthError> {
        let request = crate::http::client()
            .post(&self.api_key_mint_url)
            .header("Accept", "application/json")
            .header("Authorization", format!("Bearer {identity}"))
            .header("Content-Type", "application/json")
            .header("x-api-version", "1.0.0")
            .timeout(REQUEST_TIMEOUT)
            .body("{}");
        let response = send(request, cancel).await?;
        let status = response.status().as_u16();
        let json = json_body(response).await;
        if status == 401 || status == 403 {
            return Err(AuthError::Failed(format!(
                "Meta session expired (status {status}). Run `/login meta` to sign in again.{}",
                error_detail(&json)
            )));
        }
        if !(200..300).contains(&status) {
            return Err(AuthError::Failed(format!(
                "Meta API key mint failed with status {status}{}",
                error_detail(&json)
            )));
        }
        let Some(key) = json["api_key"].as_str().filter(|key| !key.is_empty()) else {
            let action = trusted_url(&json["action_url"])
                .map(|url| format!(" Complete setup at {url}"))
                .unwrap_or_default();
            return Err(AuthError::Failed(format!(
                "Meta did not issue an API key.{action}"
            )));
        };
        Ok(OAuthCredential {
            access: key.to_owned(),
            refresh: identity.to_owned(),
            expires: now_ms() + API_KEY_LIFETIME_MS,
            extra: Map::new(),
        })
    }

    async fn login_meta(&self, interaction: &Interaction) -> Result<OAuthCredential, AuthError> {
        let cancel = interaction.cancel();
        let response = send(
            post_form(&self.device_authorization_url, &[("client_id", CLIENT_ID)]),
            cancel,
        )
        .await?;
        let status = response.status().as_u16();
        let json = json_body(response).await;
        if !(200..300).contains(&status) {
            return Err(AuthError::Failed(format!(
                "Meta device authorization failed with status {status}{}",
                error_detail(&json)
            )));
        }
        let verification = trusted_url(&json["verification_uri_complete"])
            .or_else(|| trusted_url(&json["verification_uri"]));
        let (Some(device_code), Some(user_code), Some(verification)) = (
            json["device_code"].as_str().filter(|code| !code.is_empty()),
            json["user_code"].as_str().filter(|code| !code.is_empty()),
            verification,
        ) else {
            return Err(AuthError::Failed(format!(
                "Invalid Meta device authorization response: {}",
                yapi_types::json::to_string(&json).unwrap_or_default()
            )));
        };
        let interval = positive(&json["interval"]);
        let expires_in = positive(&json["expires_in"]);
        interaction.notify(AuthEvent::DeviceCode {
            user_code: user_code.to_owned(),
            verification_uri: verification,
            interval_seconds: interval,
            expires_in_seconds: expires_in,
        });
        let identity = poll_device_code(interval, expires_in, true, cancel, || {
            let request = post_form(
                &self.device_token_url,
                &[
                    ("grant_type", DEVICE_CODE_GRANT),
                    ("device_code", device_code),
                    ("client_id", CLIENT_ID),
                ],
            );
            async move {
                let response = send(request, cancel).await?;
                let status = response.status().as_u16();
                let json = json_body(response).await;
                if (200..300).contains(&status)
                    && let Some(token) = json["access_token"].as_str().filter(|t| !t.is_empty())
                {
                    return Ok(Poll::Complete(token.to_owned()));
                }
                Ok(match json["error"].as_str() {
                    Some("authorization_pending") => Poll::Pending,
                    Some("slow_down") => Poll::SlowDown(positive(&json["interval"])),
                    Some("access_denied") => Poll::Failed("Meta login was denied.".into()),
                    Some("expired_token") => Poll::Failed(
                        "Meta device authorization expired. Please restart login.".into(),
                    ),
                    _ => Poll::Failed(format!(
                        "Meta device token request failed with status {status}{}",
                        error_detail(&json)
                    )),
                })
            }
        })
        .await?;
        interaction.notify(AuthEvent::Progress {
            message: "Enabling Meta Model API access...".into(),
        });
        self.mint(&identity, cancel).await
    }
}

impl OAuthProvider for MetaOAuth {
    fn name(&self) -> &str {
        "Meta (Muse subscription)"
    }

    fn login_label(&self) -> Option<&str> {
        Some("Sign in with Meta")
    }

    fn login<'a>(
        &'a self,
        interaction: &'a Interaction,
        _options: &'a LoginOptions,
    ) -> BoxFuture<'a, Result<OAuthCredential, AuthError>> {
        Box::pin(self.login_meta(interaction))
    }

    fn refresh<'a>(
        &'a self,
        credential: &'a OAuthCredential,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<OAuthCredential, AuthError>> {
        Box::pin(self.mint(&credential.refresh, cancel))
    }
}
