//! GitHub Copilot sign-in: a GitHub device flow, then a Copilot token whose
//! `proxy-ep` names the account's API endpoint. Port of
//! `oauth/github-copilot.ts`.

use std::collections::HashSet;
use std::time::Duration;

use ri_types::auth::OAuthCredential;
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use super::device::{Poll, poll_device_code};
use super::{
    AuthError, AuthEvent, AuthPrompt, BoxFuture, Interaction, LoginOptions, OAuthAuth,
    OAuthProvider, form, json_body, send,
};

/// Public client id, base64 in pi's source.
const CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
const USER_AGENT: &str = "GitHubCopilotChat/0.35.0";
const HEADERS: [(&str, &str); 4] = [
    ("User-Agent", USER_AGENT),
    ("Editor-Version", "vscode/1.107.0"),
    ("Editor-Plugin-Version", "copilot-chat/0.35.0"),
    ("Copilot-Integration-Id", "vscode-chat"),
];
const API_VERSION: &str = "2026-06-01";
const DEFAULT_BASE_URL: &str = "https://api.individual.githubcopilot.com";
const EXPIRY_MARGIN_MS: u64 = 5 * 60 * 1000;

/// The Copilot sign-in. Endpoints derive from the GitHub domain unless
/// overridden.
#[derive(Clone, Debug, Default)]
pub struct CopilotOAuth {
    /// Replaces `https://<domain>` and `https://api.<domain>` for GitHub calls.
    pub github_url: Option<String>,
    /// Replaces the Copilot API base URL.
    pub api_url: Option<String>,
}

/// The host of a GitHub Enterprise URL or domain.
fn normalize_domain(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let candidate = if trimmed.contains("://") {
        trimmed.to_owned()
    } else {
        format!("https://{trimmed}")
    };
    url::Url::parse(&candidate)
        .ok()?
        .host_str()
        .map(str::to_owned)
}

/// The API base URL from a Copilot token's `proxy-ep` field.
fn base_url_from_token(token: &str) -> Option<String> {
    let start = token.find("proxy-ep=")? + "proxy-ep=".len();
    let host = token[start..].split(';').next()?;
    if host.is_empty() {
        return None;
    }
    let host = host
        .strip_prefix("proxy.")
        .map_or_else(|| host.to_owned(), |rest| format!("api.{rest}"));
    Some(format!("https://{host}"))
}

fn enterprise_domain(credential: &OAuthCredential) -> Option<String> {
    credential
        .extra
        .get("enterpriseUrl")
        .and_then(Value::as_str)
        .filter(|url| !url.is_empty())
        .and_then(normalize_domain)
}

/// What the account's model catalog allows.
#[derive(Debug, Default, PartialEq)]
struct Catalog {
    available: Vec<String>,
    policy: Vec<String>,
}

fn parse_catalog(raw: &Value, allow_policy_fallback: bool) -> Result<Catalog, AuthError> {
    let data = raw
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| AuthError::failed("Invalid Copilot models response"))?;
    struct Entry {
        id: String,
        picker: bool,
        policy: Option<String>,
    }
    let entries: Vec<Entry> = data
        .iter()
        .filter_map(|item| {
            let id = item.get("id")?.as_str()?.to_owned();
            let tool_calls = item.pointer("/capabilities/supports/tool_calls");
            if tool_calls == Some(&Value::Bool(false)) {
                return None;
            }
            Some(Entry {
                id,
                picker: item.get("model_picker_enabled") == Some(&Value::Bool(true)),
                policy: item
                    .pointer("/policy/state")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            })
        })
        .collect();
    let picker: Vec<String> = entries
        .iter()
        .filter(|entry| entry.picker && entry.policy.as_deref() != Some("disabled"))
        .map(|entry| entry.id.clone())
        .collect();
    let use_policy_fallback = allow_policy_fallback && picker.is_empty();
    let available = if !picker.is_empty() || !allow_policy_fallback {
        picker
    } else {
        entries
            .iter()
            .filter(|entry| entry.policy.as_deref() == Some("enabled"))
            .map(|entry| entry.id.clone())
            .collect()
    };
    let known: HashSet<String> = crate::catalog::builtin_models("github-copilot")
        .into_iter()
        .map(|model| model.id)
        .collect();
    let policy = entries
        .iter()
        .filter(|entry| {
            entry.policy.as_deref() == Some("unconfigured")
                && known.contains(&entry.id)
                && (entry.picker || use_policy_fallback)
        })
        .map(|entry| entry.id.clone())
        .collect();
    Ok(Catalog { available, policy })
}

async fn fetch_json(
    request: reqwest::RequestBuilder,
    cancel: &CancellationToken,
) -> Result<Value, AuthError> {
    let response = send(request, cancel).await?;
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(AuthError::Failed(format!(
            "{} {}: {text}",
            status.as_u16(),
            status.canonical_reason().unwrap_or_default()
        )));
    }
    Ok(json_body(response).await)
}

/// Sends with a five-second timeout per attempt, retrying 429 responses
/// within `max_retries` and `budget`.
async fn send_with_rate_limit_retry(
    build: impl Fn() -> reqwest::RequestBuilder,
    cancel: &CancellationToken,
    max_retries: u32,
    budget: Duration,
) -> Result<reqwest::Response, AuthError> {
    let deadline = tokio::time::Instant::now() + budget;
    let mut retry = 0;
    loop {
        let response = send(build().timeout(Duration::from_secs(5)), cancel).await?;
        if response.status().as_u16() != 429 || retry == max_retries {
            return Ok(response);
        }
        let delay = match response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
        {
            Some(value) => match value.trim().parse::<f64>() {
                Ok(seconds) => {
                    Duration::try_from_secs_f64(seconds.max(0.0)).unwrap_or(Duration::MAX)
                }
                Err(_) => return Ok(response),
            },
            None => Duration::from_millis(500 * 2u64.pow(retry)),
        };
        if delay >= deadline.saturating_duration_since(tokio::time::Instant::now()) {
            return Ok(response);
        }
        retry += 1;
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            () = cancel.cancelled() => return Err(AuthError::Cancelled),
        }
    }
}

impl CopilotOAuth {
    fn github(&self, domain: &str) -> String {
        self.github_url
            .clone()
            .unwrap_or_else(|| format!("https://{domain}"))
    }

    fn github_api(&self, domain: &str) -> String {
        self.github_url
            .clone()
            .unwrap_or_else(|| format!("https://api.{domain}"))
    }

    fn base_url(&self, token: &str, enterprise: Option<&str>) -> String {
        if let Some(url) = &self.api_url {
            return url.clone();
        }
        base_url_from_token(token)
            .or_else(|| enterprise.map(|domain| format!("https://copilot-api.{domain}")))
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned())
    }

    async fn copilot_token(
        &self,
        github_token: &str,
        enterprise: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<OAuthCredential, AuthError> {
        let domain = enterprise.unwrap_or("github.com");
        let mut request = crate::http::client()
            .get(format!(
                "{}/copilot_internal/v2/token",
                self.github_api(domain)
            ))
            .header("Accept", "application/json")
            .header("Authorization", format!("Bearer {github_token}"));
        for (name, value) in HEADERS {
            request = request.header(name, value);
        }
        let raw = fetch_json(request, cancel).await?;
        if !raw.is_object() {
            return Err(AuthError::failed("Invalid Copilot token response"));
        }
        let (Some(token), Some(expires_at)) = (
            raw.get("token").and_then(Value::as_str),
            raw.get("expires_at").and_then(Value::as_f64),
        ) else {
            return Err(AuthError::failed("Invalid Copilot token response fields"));
        };
        let mut extra = Map::new();
        if let Some(domain) = enterprise {
            extra.insert("enterpriseUrl".into(), Value::String(domain.to_owned()));
        }
        let expires = (expires_at * 1000.0).max(0.0) as u64;
        Ok(OAuthCredential {
            access: token.to_owned(),
            refresh: github_token.to_owned(),
            expires: expires.saturating_sub(EXPIRY_MARGIN_MS),
            extra,
        })
    }

    async fn models(
        &self,
        token: &str,
        enterprise: Option<&str>,
        cancel: &CancellationToken,
        max_retries: u32,
        budget: Duration,
    ) -> Result<Catalog, AuthError> {
        let base = self.base_url(token, enterprise);
        // Some Individual accounts disable every picker flag despite enabled
        // policies; only that endpoint falls back to the policies.
        let allow_policy_fallback = base == DEFAULT_BASE_URL;
        let build = || {
            let mut request = crate::http::client()
                .get(format!("{base}/models"))
                .header("Accept", "application/json")
                .header("Authorization", format!("Bearer {token}"));
            for (name, value) in HEADERS {
                request = request.header(name, value);
            }
            request.header("X-GitHub-Api-Version", API_VERSION)
        };
        let response = send_with_rate_limit_retry(build, cancel, max_retries, budget).await?;
        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(AuthError::Failed(format!(
                "{} {}: {text}",
                status.as_u16(),
                status.canonical_reason().unwrap_or_default()
            )));
        }
        parse_catalog(&json_body(response).await, allow_policy_fallback)
    }

    /// Enables models for the account, best effort; exhausted rate limiting
    /// stops the batch.
    async fn enable_models(
        &self,
        token: &str,
        ids: &[String],
        enterprise: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<Vec<String>, AuthError> {
        let base = self.base_url(token, enterprise);
        let mut enabled = Vec::new();
        for id in ids {
            let build = || {
                let mut request = crate::http::client()
                    .post(format!("{base}/models/{id}/policy"))
                    .header("Content-Type", "application/json")
                    .header("Authorization", format!("Bearer {token}"));
                for (name, value) in HEADERS {
                    request = request.header(name, value);
                }
                request
                    .header("openai-intent", "chat-policy")
                    .header("x-interaction-type", "chat-policy")
                    .body("{\"state\":\"enabled\"}")
            };
            match send_with_rate_limit_retry(build, cancel, 2, Duration::from_secs(5)).await {
                Ok(response) if response.status().as_u16() == 429 => break,
                Ok(response) if response.status().is_success() => enabled.push(id.clone()),
                Ok(_) => {}
                Err(AuthError::Cancelled) => return Err(AuthError::Cancelled),
                Err(_) => {}
            }
        }
        Ok(enabled)
    }

    async fn login_copilot(&self, interaction: &Interaction) -> Result<OAuthCredential, AuthError> {
        let input = interaction
            .prompt(AuthPrompt::Text {
                message: "GitHub Enterprise URL/domain (blank for github.com)".into(),
                placeholder: Some("company.ghe.com".into()),
            })
            .await?;
        interaction.check()?;
        let enterprise = normalize_domain(&input);
        if !input.trim().is_empty() && enterprise.is_none() {
            return Err(AuthError::failed("Invalid GitHub Enterprise URL/domain"));
        }
        let domain = enterprise.clone().unwrap_or_else(|| "github.com".into());
        let cancel = interaction.cancel();

        let request = crate::http::client()
            .post(format!("{}/login/device/code", self.github(&domain)))
            .header("Accept", "application/json")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("User-Agent", USER_AGENT)
            .body(form(&[("client_id", CLIENT_ID), ("scope", "read:user")]));
        let device = fetch_json(request, cancel).await?;
        if !device.is_object() {
            return Err(AuthError::failed("Invalid device code response"));
        }
        let interval = device.get("interval");
        let (Some(device_code), Some(user_code), Some(verification_uri), Some(expires_in)) = (
            device.get("device_code").and_then(Value::as_str),
            device.get("user_code").and_then(Value::as_str),
            device.get("verification_uri").and_then(Value::as_str),
            device.get("expires_in").and_then(Value::as_f64),
        ) else {
            return Err(AuthError::failed("Invalid device code response fields"));
        };
        if interval.is_some_and(|interval| !interval.is_number()) {
            return Err(AuthError::failed("Invalid device code response fields"));
        }
        let interval = interval.and_then(Value::as_f64);
        // The URI is opened in a browser; only web URLs are trusted.
        let verification_uri = url::Url::parse(verification_uri)
            .ok()
            .filter(|url| matches!(url.scheme(), "https" | "http"))
            .ok_or_else(|| {
                AuthError::failed("Untrusted verification_uri in device code response")
            })?;
        interaction.notify(AuthEvent::DeviceCode {
            user_code: user_code.to_owned(),
            verification_uri: verification_uri.to_string(),
            interval_seconds: interval,
            expires_in_seconds: Some(expires_in),
        });

        let token_url = format!("{}/login/oauth/access_token", self.github(&domain));
        let body = form(&[
            ("client_id", CLIENT_ID),
            ("device_code", device_code),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ]);
        let github_token = poll_device_code(interval, Some(expires_in), true, cancel, || {
            let request = crate::http::client()
                .post(&token_url)
                .header("Accept", "application/json")
                .header("Content-Type", "application/x-www-form-urlencoded")
                .header("User-Agent", USER_AGENT)
                .body(body.clone());
            async move {
                let raw = fetch_json(request, cancel).await?;
                if let Some(token) = raw.get("access_token").and_then(Value::as_str) {
                    return Ok(Poll::Complete(token.to_owned()));
                }
                let Some(error) = raw.get("error").and_then(Value::as_str) else {
                    return Ok(Poll::Failed("Invalid device token response".into()));
                };
                Ok(match error {
                    "authorization_pending" => Poll::Pending,
                    "slow_down" => Poll::SlowDown(raw.get("interval").and_then(Value::as_f64)),
                    _ => {
                        let suffix = raw
                            .get("error_description")
                            .and_then(Value::as_str)
                            .map_or_else(String::new, |text| format!(": {text}"));
                        Poll::Failed(format!("Device flow failed: {error}{suffix}"))
                    }
                })
            }
        })
        .await?;

        let mut credential = self
            .copilot_token(&github_token, enterprise.as_deref(), cancel)
            .await?;
        let catalog = self
            .models(
                &credential.access,
                enterprise.as_deref(),
                cancel,
                2,
                Duration::from_secs(5),
            )
            .await?;
        let mut enabled = Vec::new();
        if !catalog.policy.is_empty() {
            interaction.notify(AuthEvent::Progress {
                message: "Enabling models...".into(),
            });
            enabled = self
                .enable_models(
                    &credential.access,
                    &catalog.policy,
                    enterprise.as_deref(),
                    cancel,
                )
                .await?;
        }
        let mut ids = catalog.available;
        for id in enabled {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        credential.extra.insert(
            "availableModelIds".into(),
            Value::Array(ids.into_iter().map(Value::String).collect()),
        );
        Ok(credential)
    }
}

impl OAuthProvider for CopilotOAuth {
    fn name(&self) -> &str {
        "GitHub Copilot"
    }

    fn login<'a>(
        &'a self,
        interaction: &'a Interaction,
        _options: &'a LoginOptions,
    ) -> BoxFuture<'a, Result<OAuthCredential, AuthError>> {
        Box::pin(self.login_copilot(interaction))
    }

    fn refresh<'a>(
        &'a self,
        credential: &'a OAuthCredential,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<OAuthCredential, AuthError>> {
        Box::pin(async move {
            let enterprise = enterprise_domain(credential);
            let mut refreshed = self
                .copilot_token(&credential.refresh, enterprise.as_deref(), cancel)
                .await?;
            let catalog = self
                .models(
                    &refreshed.access,
                    enterprise.as_deref(),
                    cancel,
                    0,
                    Duration::ZERO,
                )
                .await?;
            refreshed.extra.insert(
                "availableModelIds".into(),
                Value::Array(catalog.available.into_iter().map(Value::String).collect()),
            );
            Ok(refreshed)
        })
    }

    fn to_auth(&self, credential: &OAuthCredential) -> OAuthAuth {
        OAuthAuth {
            api_key: credential.access.clone(),
            base_url: Some(
                self.base_url(&credential.access, enterprise_domain(credential).as_deref()),
            ),
        }
    }
}

/// The models a Copilot credential allows, when it lists them.
pub fn available_model_ids(credential: &OAuthCredential) -> Option<HashSet<String>> {
    let ids = credential.extra.get("availableModelIds")?.as_array()?;
    ids.iter()
        .map(|id| id.as_str().map(str::to_owned))
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn derives_endpoints_like_pi() {
        assert_eq!(
            base_url_from_token("tid=1;exp=2;proxy-ep=proxy.business.githubcopilot.com;st=x"),
            Some("https://api.business.githubcopilot.com".into())
        );
        assert_eq!(base_url_from_token("tid=1"), None);
        assert_eq!(
            normalize_domain("https://ghe.example.com/x"),
            Some("ghe.example.com".into())
        );
        assert_eq!(
            normalize_domain("company.ghe.com"),
            Some("company.ghe.com".into())
        );
        assert_eq!(normalize_domain("  "), None);
        let oauth = CopilotOAuth::default();
        assert_eq!(
            oauth.base_url("tid=1", Some("ghe.example.com")),
            "https://copilot-api.ghe.example.com"
        );
        assert_eq!(oauth.base_url("tid=1", None), DEFAULT_BASE_URL);
    }

    #[test]
    fn reads_the_model_catalog() {
        let known = crate::catalog::builtin_models("github-copilot")[0]
            .id
            .clone();
        let raw = json!({"data": [
            {"id": "a", "model_picker_enabled": true, "policy": {"state": "enabled"}},
            {"id": "b", "model_picker_enabled": true, "policy": {"state": "disabled"}},
            {"id": "c", "model_picker_enabled": true, "capabilities": {"supports": {"tool_calls": false}}},
            {"id": known, "model_picker_enabled": true, "policy": {"state": "unconfigured"}},
        ]});
        let catalog = parse_catalog(&raw, true).unwrap();
        assert_eq!(catalog.available, ["a".to_owned(), known.clone()]);
        assert_eq!(catalog.policy, [known]);
        let fallback = json!({"data": [
            {"id": "a", "model_picker_enabled": false, "policy": {"state": "enabled"}},
        ]});
        assert_eq!(parse_catalog(&fallback, true).unwrap().available, ["a"]);
        assert!(
            parse_catalog(&fallback, false)
                .unwrap()
                .available
                .is_empty()
        );
    }
}
