//! Anthropic workload identity federation: exchanges an external OIDC token
//! for a short-lived Anthropic access token and caches it per configuration.
//!
//! Port of the Anthropic SDK's `oidcFederationProvider` and `TokenCache`
//! (`@anthropic-ai/sdk` 0.124.0), which pi `v1.0.0` uses when the
//! `ANTHROPIC_FEDERATION_RULE_ID`, `ANTHROPIC_ORGANIZATION_ID` and
//! `ANTHROPIC_IDENTITY_TOKEN_FILE` provider settings are set.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::credentials::{ProviderEnv, provider_env_value};

const GRANT_TYPE_JWT_BEARER: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";
const TOKEN_ENDPOINT: &str = "/v1/oauth/token";
/// The `anthropic-beta` value requests with a federated bearer token carry.
pub const OAUTH_API_BETA: &str = "oauth-2025-04-20";
const FEDERATION_BETA: &str = "oidc-federation-2026-04-01";
const SDK_VERSION: &str = "0.124.0";
/// A cached token with less validity left is exchanged again.
const MANDATORY_REFRESH_SECONDS: u64 = 30;
const MAX_ASSERTION_BYTES: usize = 16 * 1024;
const MAX_ERROR_BODY_CHARS: usize = 2000;

/// What the exchange needs, from the provider settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    base_url: String,
    federation_rule_id: String,
    organization_id: String,
    identity_token_file: String,
    service_account_id: Option<String>,
    workspace_id: Option<String>,
}

impl Config {
    /// The federation configuration for a request to `base_url`, when the
    /// three required settings are present.
    pub fn from_env(base_url: &str, env: Option<&ProviderEnv>) -> Option<Config> {
        let value = |name: &str| provider_env_value(name, env);
        Some(Config {
            base_url: base_url.trim_end_matches('/').to_owned(),
            federation_rule_id: value("ANTHROPIC_FEDERATION_RULE_ID")?,
            organization_id: value("ANTHROPIC_ORGANIZATION_ID")?,
            identity_token_file: value("ANTHROPIC_IDENTITY_TOKEN_FILE")?,
            service_account_id: value("ANTHROPIC_SERVICE_ACCOUNT_ID"),
            workspace_id: value("ANTHROPIC_WORKSPACE_ID"),
        })
    }

    fn key(&self) -> String {
        format!("{self:?}")
    }
}

struct Cached {
    token: String,
    expires_at: u64,
}

fn cache() -> &'static Mutex<HashMap<String, Cached>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Cached>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn now_seconds() -> u64 {
    super::now_ms() / 1000
}

/// An access token for `config`: the cached one while it has more than 30
/// seconds left, else a fresh exchange. `force` skips the cache, as after a
/// 401.
pub async fn token(
    config: &Config,
    force: bool,
    cancel: &CancellationToken,
) -> Result<String, String> {
    if !force
        && let Ok(cache) = cache().lock()
        && let Some(cached) = cache.get(&config.key())
        && cached.expires_at > now_seconds() + MANDATORY_REFRESH_SECONDS
    {
        return Ok(cached.token.clone());
    }
    let (token, expires_in) = exchange(config, cancel).await?;
    if let Ok(mut cache) = cache().lock() {
        cache.insert(
            config.key(),
            Cached {
                token: token.clone(),
                expires_at: now_seconds() + expires_in,
            },
        );
    }
    Ok(token)
}

/// Drops the cached token for `config`.
pub fn invalidate(config: &Config) {
    if let Ok(mut cache) = cache().lock() {
        cache.remove(&config.key());
    }
}

/// The SDK's `requireSecureTokenEndpoint`: the assertion travels only over
/// HTTPS, or plain HTTP to a loopback host.
fn require_secure(base_url: &str) -> Result<(), String> {
    let url = url::Url::parse(base_url)
        .map_err(|err| format!("Invalid token endpoint base URL \"{base_url}\": {err}"))?;
    if url.scheme() == "https" {
        return Ok(());
    }
    let host = url
        .host_str()
        .unwrap_or_default()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_lowercase();
    if url.scheme() == "http" && matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1") {
        return Ok(());
    }
    Err(format!(
        "Refusing to send credential over non-https token endpoint \"{base_url}\""
    ))
}

/// The SDK's `redactSensitive`: JSON error bodies keep only RFC 6749 error
/// fields; other text is truncated.
fn redact(body: &str) -> String {
    match serde_json::from_str::<Value>(body) {
        Ok(Value::Object(fields)) => {
            let safe: Map<String, Value> = fields
                .into_iter()
                .filter(|(key, _)| {
                    matches!(key.as_str(), "error" | "error_description" | "error_uri")
                })
                .collect();
            yapi_types::json::to_string(&Value::Object(safe)).unwrap_or_default()
        }
        Ok(_) => "null".to_owned(),
        Err(_) if body.chars().count() <= MAX_ERROR_BODY_CHARS => body.to_owned(),
        Err(_) => {
            let total = body.chars().count();
            let head: String = body.chars().take(MAX_ERROR_BODY_CHARS).collect();
            format!("{head}... <{} more chars>", total - MAX_ERROR_BODY_CHARS)
        }
    }
}

async fn exchange(config: &Config, cancel: &CancellationToken) -> Result<(String, u64), String> {
    require_secure(&config.base_url)?;
    let path = &config.identity_token_file;
    let content = tokio::fs::read_to_string(path)
        .await
        .map_err(|err| format!("Failed to read identity token file at {path}: {err}"))?;
    let assertion = content.trim();
    if assertion.is_empty() {
        return Err(format!("Identity token file at {path} is empty"));
    }
    if assertion.len() > MAX_ASSERTION_BYTES {
        return Err(format!(
            "Identity token is {} KiB, exceeds the 16 KiB assertion limit",
            assertion.len().div_ceil(1024)
        ));
    }
    let mut body = Map::new();
    body.insert("grant_type".into(), GRANT_TYPE_JWT_BEARER.into());
    body.insert("assertion".into(), assertion.into());
    body.insert(
        "federation_rule_id".into(),
        config.federation_rule_id.clone().into(),
    );
    body.insert(
        "organization_id".into(),
        config.organization_id.clone().into(),
    );
    if let Some(id) = &config.service_account_id {
        body.insert("service_account_id".into(), id.clone().into());
    }
    if let Some(id) = &config.workspace_id {
        body.insert("workspace_id".into(), id.clone().into());
    }
    let url = format!("{}{TOKEN_ENDPOINT}", config.base_url);
    let request = crate::http::client()
        .post(&url)
        .header("Content-Type", "application/json")
        .header(
            "anthropic-beta",
            format!("{OAUTH_API_BETA},{FEDERATION_BETA}"),
        )
        .header(
            "User-Agent",
            format!("anthropic-sdk-typescript/{SDK_VERSION} oidcFederationProvider"),
        )
        .body(yapi_types::json::to_string(&Value::Object(body)).unwrap_or_default());
    let response = super::send(request, cancel)
        .await
        .map_err(|err| match err {
            super::AuthError::Cancelled => crate::http::ABORTED_BEFORE_RESPONSE.to_owned(),
            super::AuthError::Failed(_) => {
                format!("Failed to reach token endpoint {url}: TypeError: fetch failed")
            }
        })?;
    let status = response.status().as_u16();
    let request_id = response
        .headers()
        .get("Request-Id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let text = response.text().await.unwrap_or_default();
    if !(200..300).contains(&status) {
        let mut hint = String::new();
        if status == 401 {
            let workspace = if config.workspace_id.is_some() {
                ""
            } else {
                "If your federation rule is scoped to multiple workspaces, set the ANTHROPIC_WORKSPACE_ID environment variable, the 'workspace_id' config key, or the `workspaceId` option. "
            };
            hint = format!(
                " Ensure your federation rule matches your identity token. {workspace}View your authentication events in the Workload identity page of Claude Console for more details."
            );
        }
        let id = request_id
            .map(|id| format!(" (request-id {id})"))
            .unwrap_or_default();
        return Err(format!(
            "Token exchange failed with status {status}{id}: {}{hint}",
            redact(&text)
        ));
    }
    let data: Value = serde_json::from_str(&text)
        .map_err(|_| format!("Token endpoint returned non-JSON response (status {status})"))?;
    let token = data["access_token"]
        .as_str()
        .filter(|token| !token.is_empty())
        .ok_or_else(|| {
            format!(
                "Token endpoint response missing access_token: {}",
                redact(&text)
            )
        })?;
    if let Some(kind) = data["token_type"].as_str()
        && !kind.is_empty()
        && !kind.eq_ignore_ascii_case("bearer")
    {
        return Err(format!(
            "Token endpoint response: unsupported token_type \"{kind}\" (want Bearer)"
        ));
    }
    let expires_in = match &data["expires_in"] {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    }
    .filter(|seconds| seconds.is_finite())
    .ok_or_else(|| {
        format!(
            "Token endpoint response missing required fields: {}",
            redact(&text)
        )
    })?;
    Ok((token.to_owned(), expires_in.max(0.0) as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_settings_and_guards_the_endpoint() {
        let mut env = ProviderEnv::new();
        env.insert("ANTHROPIC_FEDERATION_RULE_ID".into(), "rule".into());
        env.insert("ANTHROPIC_ORGANIZATION_ID".into(), "org".into());
        assert_eq!(
            Config::from_env("https://api.anthropic.com", Some(&env)),
            None
        );
        env.insert("ANTHROPIC_IDENTITY_TOKEN_FILE".into(), "/t".into());
        let config = Config::from_env("https://api.anthropic.com/", Some(&env)).unwrap();
        assert_eq!(config.base_url, "https://api.anthropic.com");
        assert!(require_secure("http://127.0.0.1:9").is_ok());
        assert!(require_secure("http://[::1]:9").is_ok());
        assert_eq!(
            require_secure("http://example.com"),
            Err(
                "Refusing to send credential over non-https token endpoint \"http://example.com\""
                    .into()
            )
        );
        assert_eq!(
            redact(r#"{"error":"invalid_grant","assertion":"secret"}"#),
            r#"{"error":"invalid_grant"}"#
        );
    }
}
