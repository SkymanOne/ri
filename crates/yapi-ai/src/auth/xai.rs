//! xAI (Grok/X subscription): an RFC 8628 device authorization against
//! auth.x.ai. Port of `oauth/xai.ts`.

use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;
use yapi_types::auth::OAuthCredential;

use super::device::{DEVICE_CODE_GRANT, Poll, poll_device_code};
use super::{
    AuthError, AuthEvent, BoxFuture, Interaction, LoginOptions, OAuthProvider, form, now_ms, send,
};

const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
/// Refresh this long before the reported expiry.
const REFRESH_SKEW_MS: u64 = 5 * 60 * 1000;
const DEFAULT_TOKEN_LIFETIME_SECONDS: f64 = 3600.0;

/// The xAI sign-in and its endpoints.
#[derive(Clone, Debug)]
pub struct XaiOAuth {
    /// Device authorization endpoint.
    pub device_code_url: String,
    /// Token endpoint.
    pub token_url: String,
}

impl Default for XaiOAuth {
    fn default() -> XaiOAuth {
        XaiOAuth {
            device_code_url: "https://auth.x.ai/oauth2/device/code".into(),
            token_url: "https://auth.x.ai/oauth2/token".into(),
        }
    }
}

/// A response: success, status and JSON object body.
struct Answer {
    ok: bool,
    status: u16,
    body: Value,
}

async fn post_form(
    url: &str,
    fields: &[(&str, &str)],
    cancel: &CancellationToken,
) -> Result<Answer, AuthError> {
    let request = crate::http::client()
        .post(url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(form(fields));
    let response = send(request, cancel).await?;
    let status = response.status().as_u16();
    let bytes = response.bytes().await.unwrap_or_default();
    let body = match serde_json::from_slice::<Value>(&bytes) {
        Ok(object @ Value::Object(_)) => object,
        Ok(_) => Value::Object(Map::new()),
        Err(_) if cancel.is_cancelled() => return Err(AuthError::Cancelled),
        Err(_) => {
            return Err(AuthError::Failed(format!(
                "xAI OAuth returned invalid JSON (HTTP {status})"
            )));
        }
    };
    Ok(Answer {
        ok: (200..300).contains(&status),
        status,
        body,
    })
}

fn failure(action: &str, answer: &Answer) -> String {
    let detail: Vec<&str> = ["error", "error_description"]
        .iter()
        .filter_map(|key| answer.body[*key].as_str().filter(|value| !value.is_empty()))
        .collect();
    let detail = detail.join(": ");
    let detail = if detail.is_empty() {
        String::new()
    } else {
        format!(": {detail}")
    };
    format!("xAI OAuth {action} failed (HTTP {}){detail}", answer.status)
}

fn required<'a>(body: &'a Value, field: &str) -> Result<&'a str, AuthError> {
    body[field]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AuthError::Failed(format!("Invalid xAI OAuth response field: {field}")))
}

fn positive(body: &Value, field: &str) -> Result<f64, AuthError> {
    body[field]
        .as_f64()
        .filter(|value| value.is_finite() && *value > 0.0)
        .ok_or_else(|| AuthError::Failed(format!("Invalid xAI OAuth response field: {field}")))
}

/// An https URL, the only kind opened in the browser.
fn verification_uri(raw: &str) -> Result<String, AuthError> {
    match url::Url::parse(raw) {
        Ok(url) if url.scheme() == "https" => Ok(url.to_string()),
        _ => Err(AuthError::failed(
            "Untrusted verification URI in xAI OAuth response",
        )),
    }
}

fn credential(body: &Value, previous_refresh: Option<&str>) -> Result<OAuthCredential, AuthError> {
    let access = required(body, "access_token")?.to_owned();
    let refresh = match (&body["refresh_token"], previous_refresh) {
        (Value::Null, Some(previous)) => previous.to_owned(),
        _ => required(body, "refresh_token")?.to_owned(),
    };
    let expires_in = if body["expires_in"].is_null() {
        DEFAULT_TOKEN_LIFETIME_SECONDS
    } else {
        positive(body, "expires_in")?
    };
    Ok(OAuthCredential {
        access,
        refresh,
        expires: (now_ms() + (expires_in * 1000.0) as u64).saturating_sub(REFRESH_SKEW_MS),
        extra: Map::new(),
    })
}

impl XaiOAuth {
    async fn login_xai(&self, interaction: &Interaction) -> Result<OAuthCredential, AuthError> {
        let cancel = interaction.cancel();
        let answer = post_form(
            &self.device_code_url,
            &[
                ("client_id", CLIENT_ID),
                ("scope", SCOPE),
                ("referrer", "pi"),
            ],
            cancel,
        )
        .await?;
        if !answer.ok {
            return Err(AuthError::Failed(failure("device authorization", &answer)));
        }
        let body = &answer.body;
        let interval = body["interval"]
            .as_f64()
            .filter(|value| value.is_finite() && *value > 0.0);
        let complete = match body["verification_uri_complete"].as_str() {
            Some(raw) if !raw.is_empty() => Some(verification_uri(raw)?),
            _ => None,
        };
        let device_code = required(body, "device_code")?.to_owned();
        let user_code = required(body, "user_code")?.to_owned();
        let uri = verification_uri(required(body, "verification_uri")?)?;
        let expires_in = positive(body, "expires_in")?;
        interaction.notify(AuthEvent::DeviceCode {
            user_code,
            verification_uri: complete.unwrap_or(uri),
            interval_seconds: interval,
            expires_in_seconds: Some(expires_in),
        });
        poll_device_code(interval, Some(expires_in), true, cancel, || {
            let device_code = device_code.clone();
            async move {
                let answer = post_form(
                    &self.token_url,
                    &[
                        ("grant_type", DEVICE_CODE_GRANT),
                        ("client_id", CLIENT_ID),
                        ("device_code", &device_code),
                    ],
                    cancel,
                )
                .await?;
                if answer.ok {
                    return Ok(match credential(&answer.body, None) {
                        Ok(credential) => Poll::Complete(credential),
                        Err(err) => Poll::Failed(err.to_string()),
                    });
                }
                Ok(match answer.body["error"].as_str() {
                    Some("authorization_pending") => Poll::Pending,
                    Some("slow_down") => Poll::SlowDown(answer.body["interval"].as_f64()),
                    Some("access_denied" | "authorization_denied") => {
                        Poll::Failed("xAI device authorization was denied".into())
                    }
                    Some("expired_token") => Poll::Failed("xAI device code expired".into()),
                    _ => Poll::Failed(failure("device token polling", &answer)),
                })
            }
        })
        .await
    }

    async fn refresh_xai(
        &self,
        refresh: &str,
        cancel: &CancellationToken,
    ) -> Result<OAuthCredential, AuthError> {
        let answer = post_form(
            &self.token_url,
            &[
                ("grant_type", "refresh_token"),
                ("client_id", CLIENT_ID),
                ("refresh_token", refresh),
            ],
            cancel,
        )
        .await?;
        if !answer.ok {
            return Err(AuthError::Failed(failure("token refresh", &answer)));
        }
        credential(&answer.body, Some(refresh))
    }
}

impl OAuthProvider for XaiOAuth {
    fn name(&self) -> &str {
        "xAI (Grok/X subscription)"
    }

    fn login_label(&self) -> Option<&str> {
        Some("Sign in with SuperGrok or X Premium")
    }

    fn login<'a>(
        &'a self,
        interaction: &'a Interaction,
        _options: &'a LoginOptions,
    ) -> BoxFuture<'a, Result<OAuthCredential, AuthError>> {
        Box::pin(self.login_xai(interaction))
    }

    fn refresh<'a>(
        &'a self,
        credential: &'a OAuthCredential,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<OAuthCredential, AuthError>> {
        Box::pin(self.refresh_xai(&credential.refresh, cancel))
    }
}
