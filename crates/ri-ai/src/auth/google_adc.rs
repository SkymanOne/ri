//! Google Application Default Credentials: an access token for Vertex AI
//! from a credentials file.
//!
//! Port of the parts of `google-auth-library` 10.6 that pi `v1.0.0`'s Vertex
//! provider reaches: the file named by `GOOGLE_APPLICATION_CREDENTIALS` or
//! gcloud's well-known file, holding an `authorized_user`, a
//! `service_account`, an `impersonated_service_account`, or an
//! `external_account` whose subject token comes from a file or a URL.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use base64::Engine as _;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use super::{AuthError, form, now_ms};

/// The scope Vertex AI requests.
pub const CLOUD_PLATFORM_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
/// Google's OAuth 2.0 token endpoint.
pub const OAUTH2_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const JWT_BEARER: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";
const TOKEN_EXCHANGE: &str = "urn:ietf:params:oauth:grant-type:token-exchange";
const ACCESS_TOKEN_TYPE: &str = "urn:ietf:params:oauth:token-type:access_token";
/// Tokens this close to expiry are fetched again, as the library's
/// `eagerRefreshThresholdMillis`.
const EAGER_REFRESH_MS: u64 = 5 * 60 * 1000;
const NO_CREDENTIALS: &str = "Could not load the default credentials. Browse to https://cloud.google.com/docs/authentication/getting-started for more information.";

/// A token for a request: the bearer token and the quota project billed for
/// it, sent as `x-goog-user-project`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    /// The OAuth access token.
    pub access_token: String,
    /// The credentials' `quota_project_id`.
    pub quota_project: Option<String>,
}

/// The credentials file: `GOOGLE_APPLICATION_CREDENTIALS`, else gcloud's
/// `application_default_credentials.json` under `CLOUDSDK_CONFIG` or
/// `~/.config/gcloud`, if it exists. `env` reads the request's provider
/// settings, then the process environment.
pub fn credentials_file(env: impl Fn(&str) -> Option<String>) -> Option<String> {
    if let Some(path) = env("GOOGLE_APPLICATION_CREDENTIALS") {
        return Some(path);
    }
    let dir = env("CLOUDSDK_CONFIG")
        .map(std::path::PathBuf::from)
        .or_else(|| env("HOME").map(|home| std::path::Path::new(&home).join(".config/gcloud")))?;
    let path = dir.join("application_default_credentials.json");
    path.exists().then(|| path.to_string_lossy().into_owned())
}

struct Cached {
    token: Token,
    expires_at_ms: u64,
}

fn cache() -> &'static Mutex<HashMap<String, Cached>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Cached>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// An access token from the credentials file at `path`, cached until five
/// minutes before it expires. `oauth2_token_url` serves `authorized_user`
/// credentials.
pub async fn token(
    path: Option<String>,
    oauth2_token_url: &str,
    cancel: &CancellationToken,
) -> Result<Token, String> {
    let path = path.ok_or_else(|| NO_CREDENTIALS.to_owned())?;
    let text = tokio::fs::read_to_string(&path)
        .await
        .map_err(|err| file_error(&err, &path))?;
    let key = format!("{path}\0{text}");
    if let Ok(cache) = cache().lock()
        && let Some(cached) = cache.get(&key)
        && cached.expires_at_ms > now_ms() + EAGER_REFRESH_MS
    {
        return Ok(cached.token.clone());
    }
    let credentials: Value = serde_json::from_str(&text)
        .map_err(|err| format!("Unexpected token in JSON at {path}: {err}"))?;
    let (access_token, expires_at_ms) = fetch(&credentials, oauth2_token_url, cancel).await?;
    let token = Token {
        access_token,
        quota_project: credentials["quota_project_id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .map(str::to_owned),
    };
    if let Ok(mut cache) = cache().lock() {
        cache.insert(
            key,
            Cached {
                token: token.clone(),
                expires_at_ms,
            },
        );
    }
    Ok(token)
}

/// Node's message for a file that cannot be read.
fn file_error(err: &std::io::Error, path: &str) -> String {
    match err.kind() {
        std::io::ErrorKind::NotFound => {
            format!("ENOENT: no such file or directory, open '{path}'")
        }
        std::io::ErrorKind::PermissionDenied => {
            format!("EACCES: permission denied, open '{path}'")
        }
        _ => format!("{err}, open '{path}'"),
    }
}

fn field<'a>(credentials: &'a Value, name: &str) -> Result<&'a str, String> {
    credentials[name]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("The incoming JSON object does not contain a {name} field"))
}

/// The token and its expiry for one credentials object.
fn fetch<'a>(
    credentials: &'a Value,
    oauth2_token_url: &'a str,
    cancel: &'a CancellationToken,
) -> super::BoxFuture<'a, Result<(String, u64), String>> {
    Box::pin(async move {
        match credentials["type"].as_str().unwrap_or_default() {
            "authorized_user" => {
                let body = form(&[
                    ("refresh_token", field(credentials, "refresh_token")?),
                    ("client_id", field(credentials, "client_id")?),
                    ("client_secret", field(credentials, "client_secret")?),
                    ("grant_type", "refresh_token"),
                ]);
                oauth_token(oauth2_token_url, body, cancel).await
            }
            "service_account" => {
                let token_uri = credentials["token_uri"]
                    .as_str()
                    .filter(|uri| !uri.is_empty())
                    .unwrap_or(OAUTH2_TOKEN_URL);
                let assertion = signed_jwt(
                    field(credentials, "client_email")?,
                    field(credentials, "private_key")?,
                    token_uri,
                )?;
                let body = form(&[("grant_type", JWT_BEARER), ("assertion", &assertion)]);
                oauth_token(token_uri, body, cancel).await
            }
            "impersonated_service_account" => {
                let source = &credentials["source_credentials"];
                if !source.is_object() {
                    return Err(
                        "The incoming JSON object does not contain a source_credentials field"
                            .into(),
                    );
                }
                let (source_token, _) = fetch(source, oauth2_token_url, cancel).await?;
                let url = field(credentials, "service_account_impersonation_url")?;
                let delegates = credentials["delegates"].clone();
                impersonate(url, &source_token, delegates, cancel).await
            }
            "external_account" => external_account(credentials, cancel).await,
            other => Err(format!(
                "Unsupported Google credentials type for Vertex AI: {}",
                if other.is_empty() { "undefined" } else { other }
            )),
        }
    })
}

/// A self-signed RS256 JWT asking `audience` for a cloud-platform token, as
/// `gtoken` builds it.
fn signed_jwt(email: &str, private_key: &str, audience: &str) -> Result<String, String> {
    let encode = |value: &Value| {
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(ri_types::json::to_string(value).unwrap_or_default())
    };
    let issued = now_ms() / 1000;
    let header = encode(&json!({"alg": "RS256"}));
    let claims = encode(&json!({
        "iss": email,
        "scope": CLOUD_PLATFORM_SCOPE,
        "aud": audience,
        "exp": issued + 3600,
        "iat": issued,
    }));
    let message = format!("{header}.{claims}");
    let signature = rs256(private_key, message.as_bytes())?;
    Ok(format!(
        "{message}.{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature)
    ))
}

/// Signs `message` with a PEM RSA key (PKCS#8 or PKCS#1).
fn rs256(pem: &str, message: &[u8]) -> Result<Vec<u8>, String> {
    use aws_lc_rs::rand::SystemRandom;
    use aws_lc_rs::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
    let body: String = pem
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .flat_map(|line| line.trim().chars())
        .collect();
    let der = base64::engine::general_purpose::STANDARD
        .decode(body)
        .map_err(|_| "Invalid private key in service account credentials".to_owned())?;
    let key = RsaKeyPair::from_pkcs8(&der)
        .or_else(|_| RsaKeyPair::from_der(&der))
        .map_err(|_| "Invalid private key in service account credentials".to_owned())?;
    let mut signature = vec![0; key.public_modulus_len()];
    key.sign(
        &RSA_PKCS1_SHA256,
        &SystemRandom::new(),
        message,
        &mut signature,
    )
    .map_err(|_| "Could not sign the service account assertion".to_owned())?;
    Ok(signature)
}

async fn post(
    request: reqwest::RequestBuilder,
    cancel: &CancellationToken,
) -> Result<(u16, Value), String> {
    let response = super::send(request, cancel)
        .await
        .map_err(|err| match err {
            AuthError::Cancelled => crate::http::ABORTED_BEFORE_RESPONSE.to_owned(),
            AuthError::Failed(message) => message,
        })?;
    let status = response.status().as_u16();
    Ok((status, super::json_body(response).await))
}

/// The error a failed token request reports: the OAuth `error` code, else
/// the status.
fn token_error(status: u16, body: &Value) -> String {
    match &body["error"] {
        Value::String(code) => code.clone(),
        Value::Object(error) => error.get("message").and_then(Value::as_str).map_or_else(
            || format!("Request failed with status code {status}"),
            str::to_owned,
        ),
        _ => format!("Request failed with status code {status}"),
    }
}

/// An `access_token` and `expires_in` from an OAuth token endpoint.
async fn oauth_token(
    url: &str,
    body: String,
    cancel: &CancellationToken,
) -> Result<(String, u64), String> {
    let request = crate::http::client()
        .post(url)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("accept", "application/json")
        .body(body);
    let (status, body) = post(request, cancel).await?;
    if !(200..300).contains(&status) {
        return Err(token_error(status, &body));
    }
    let token = body["access_token"]
        .as_str()
        .filter(|token| !token.is_empty())
        .ok_or_else(|| "No access token in the token response".to_owned())?;
    let expires_in = body["expires_in"].as_u64().unwrap_or(3600);
    Ok((token.to_owned(), now_ms() + expires_in * 1000))
}

/// IAM Credentials' `generateAccessToken` for a service account, authorized
/// by `source_token`.
async fn impersonate(
    url: &str,
    source_token: &str,
    delegates: Value,
    cancel: &CancellationToken,
) -> Result<(String, u64), String> {
    let mut body = Map::new();
    body.insert(
        "delegates".into(),
        if delegates.is_array() {
            delegates
        } else {
            json!([])
        },
    );
    body.insert("scope".into(), json!([CLOUD_PLATFORM_SCOPE]));
    body.insert("lifetime".into(), json!("3600s"));
    let request = crate::http::client()
        .post(url)
        .header("authorization", format!("Bearer {source_token}"))
        .header("content-type", "application/json")
        .body(ri_types::json::to_string(&Value::Object(body)).unwrap_or_default());
    let (status, body) = post(request, cancel).await?;
    if !(200..300).contains(&status) {
        return Err(format!(
            "unable to impersonate: {}",
            token_error(status, &body)
        ));
    }
    let token = body["accessToken"]
        .as_str()
        .filter(|token| !token.is_empty())
        .ok_or_else(|| "No access token in the impersonation response".to_owned())?;
    let expires_at_ms = body["expireTime"]
        .as_str()
        .and_then(super::rfc3339_ms)
        .unwrap_or_else(|| now_ms() + 3600 * 1000);
    Ok((token.to_owned(), expires_at_ms))
}

/// Workload identity federation: an external subject token exchanged at the
/// Security Token Service, optionally impersonating a service account.
async fn external_account(
    credentials: &Value,
    cancel: &CancellationToken,
) -> Result<(String, u64), String> {
    let source = &credentials["credential_source"];
    let raw = if let Some(path) = source["file"].as_str() {
        tokio::fs::read_to_string(path)
            .await
            .map_err(|err| file_error(&err, path))?
    } else if let Some(url) = source["url"].as_str() {
        let mut request = crate::http::client().get(url);
        if let Some(headers) = source["headers"].as_object() {
            for (name, value) in headers {
                if let Some(value) = value.as_str() {
                    request = request.header(name.as_str(), value);
                }
            }
        }
        let response = super::send(request, cancel)
            .await
            .map_err(|err| match err {
                AuthError::Cancelled => crate::http::ABORTED_BEFORE_RESPONSE.to_owned(),
                AuthError::Failed(message) => message,
            })?;
        response.text().await.unwrap_or_default()
    } else {
        return Err(
            "Only file and URL credential sources are supported for external accounts".into(),
        );
    };
    let subject = match source["format"]["type"].as_str() {
        Some("json") => {
            let name = source["format"]["subject_token_field_name"]
                .as_str()
                .unwrap_or_default();
            let json: Value = serde_json::from_str(&raw)
                .map_err(|_| "Unable to parse the subject token response as JSON".to_owned())?;
            json[name].as_str().map(str::to_owned).ok_or_else(|| {
                format!("Unable to parse the subject_token from the credential_source {name} field")
            })?
        }
        _ => raw.trim().to_owned(),
    };
    if subject.is_empty() {
        return Err("Unable to parse the subject_token from the credential_source".into());
    }
    let token_url = credentials["token_url"]
        .as_str()
        .unwrap_or("https://sts.googleapis.com/v1/token");
    let mut pairs = vec![
        ("grant_type", TOKEN_EXCHANGE.to_owned()),
        ("audience", field(credentials, "audience")?.to_owned()),
        ("scope", CLOUD_PLATFORM_SCOPE.to_owned()),
        ("requested_token_type", ACCESS_TOKEN_TYPE.to_owned()),
        ("subject_token", subject),
        (
            "subject_token_type",
            field(credentials, "subject_token_type")?.to_owned(),
        ),
    ];
    if let Some(project) = credentials["workforce_pool_user_project"].as_str() {
        pairs.push((
            "options",
            ri_types::json::to_string(&json!({"userProject": project})).unwrap_or_default(),
        ));
    }
    let borrowed: Vec<(&str, &str)> = pairs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let (sts_token, expires_at_ms) = oauth_token(token_url, form(&borrowed), cancel).await?;
    match credentials["service_account_impersonation_url"].as_str() {
        Some(url) if !url.is_empty() => impersonate(url, &sts_token, json!([]), cancel).await,
        _ => Ok((sts_token, expires_at_ms)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh RSA key as a PKCS#8 PEM, with its public key.
    pub(crate) fn test_key() -> (String, Vec<u8>) {
        use aws_lc_rs::encoding::AsDer;
        use aws_lc_rs::rsa::{KeyPair, KeySize};
        use aws_lc_rs::signature::KeyPair as _;
        let key = KeyPair::generate(KeySize::Rsa2048).unwrap();
        let der = AsDer::<aws_lc_rs::encoding::Pkcs8V1Der<'static>>::as_der(&key).unwrap();
        let body = base64::engine::general_purpose::STANDARD.encode(der.as_ref());
        let lines: Vec<&str> = body
            .as_bytes()
            .chunks(64)
            .map(|chunk| std::str::from_utf8(chunk).unwrap())
            .collect();
        let pem = format!(
            "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
            lines.join("\n")
        );
        (pem, key.public_key().as_ref().to_vec())
    }

    #[test]
    fn signs_service_account_assertions() {
        use aws_lc_rs::signature::{RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};
        let (pem, public) = test_key();
        let jwt = signed_jwt("sa@p.iam.gserviceaccount.com", &pem, OAUTH2_TOKEN_URL).unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], "eyJhbGciOiJSUzI1NiJ9");
        let signature = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(parts[2])
            .unwrap();
        UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, &public)
            .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
            .unwrap();
        let claims: Value = serde_json::from_slice(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(parts[1])
                .unwrap(),
        )
        .unwrap();
        assert_eq!(claims["scope"], CLOUD_PLATFORM_SCOPE);
        assert_eq!(claims["aud"], OAUTH2_TOKEN_URL);
        assert_eq!(
            claims["exp"].as_u64().unwrap() - claims["iat"].as_u64().unwrap(),
            3600
        );
        assert_eq!(
            signed_jwt("x", "not a key", OAUTH2_TOKEN_URL),
            Err("Invalid private key in service account credentials".into())
        );
    }
}
