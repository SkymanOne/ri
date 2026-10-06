//! AWS credentials, request signing and event streams for Amazon Bedrock.
//!
//! The credential chain ports the AWS SDK for JavaScript v3's
//! `defaultProvider`, which pi `v1.0.0`'s Bedrock provider uses: environment
//! keys, the shared `config` and `credentials` files (static keys, assumed
//! roles, web identity, `credential_process` and IAM Identity Center), a web
//! identity token file, then container or instance metadata.

pub mod eventstream;
pub mod ini;
pub mod sigv4;

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use indexmap::IndexMap;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::auth::now_ms;
use crate::credentials::ProviderEnv;
use yapi_types::time::parse_iso;

/// AWS credentials.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Credentials {
    /// Access key id.
    pub access_key_id: String,
    /// Secret access key.
    pub secret_access_key: String,
    /// Session token of temporary credentials.
    pub session_token: Option<String>,
    /// Expiry as Unix time in milliseconds.
    pub expiration_ms: Option<u64>,
}

/// The environment the credential chain reads: the request's provider
/// settings over the process environment.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AwsEnv(IndexMap<String, String>);

impl AwsEnv {
    /// The process's `AWS_*`, `HOME` and `USERPROFILE` variables, with
    /// `overrides` on top.
    pub fn from_process(overrides: Option<&ProviderEnv>) -> AwsEnv {
        let mut vars: IndexMap<String, String> = std::env::vars()
            .filter(|(name, _)| name.starts_with("AWS_") || name == "HOME" || name == "USERPROFILE")
            .collect();
        for (name, value) in overrides.into_iter().flatten() {
            if !value.is_empty() {
                vars.insert(name.clone(), value.clone());
            }
        }
        AwsEnv(vars)
    }

    /// Exactly these variables, for tests.
    pub fn from_vars<'a>(vars: impl IntoIterator<Item = (&'a str, &'a str)>) -> AwsEnv {
        AwsEnv(
            vars.into_iter()
                .map(|(name, value)| (name.to_owned(), value.to_owned()))
                .collect(),
        )
    }

    /// A non-empty variable.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .get(name)
            .map(String::as_str)
            .filter(|value| !value.is_empty())
    }
}

/// Why credentials could not be found. `next` lets the chain try its next
/// source, as the SDK's `tryNextLink`.
struct ChainError {
    message: String,
    next: bool,
}

impl ChainError {
    fn next(message: impl Into<String>) -> ChainError {
        ChainError {
            message: message.into(),
            next: true,
        }
    }

    fn fatal(message: impl Into<String>) -> ChainError {
        ChainError {
            message: message.into(),
            next: false,
        }
    }
}

impl From<String> for ChainError {
    fn from(message: String) -> ChainError {
        ChainError::fatal(message)
    }
}

type Chained = Result<Credentials, ChainError>;

/// Credentials that expire within five minutes are fetched again, as the
/// SDK's `credentialsTreatedAsExpired`.
const EXPIRY_WINDOW_MS: u64 = 5 * 60 * 1000;

fn cache() -> &'static Mutex<HashMap<String, Credentials>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Credentials>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// The default credential chain for `profile` (else `AWS_PROFILE`), with
/// `region` for STS calls. Results are cached until five minutes before they
/// expire.
pub async fn default_credentials(
    env: &AwsEnv,
    profile: Option<&str>,
    region: Option<&str>,
    cancel: &CancellationToken,
) -> Result<Credentials, String> {
    let key = format!("{profile:?}\0{region:?}\0{:?}", env.0);
    if let Ok(cache) = cache().lock()
        && let Some(credentials) = cache.get(&key)
        && credentials
            .expiration_ms
            .is_none_or(|expires| expires > now_ms() + EXPIRY_WINDOW_MS)
    {
        return Ok(credentials.clone());
    }
    let credentials = chain(env, profile, region, cancel).await?;
    if let Ok(mut cache) = cache().lock() {
        cache.insert(key, credentials.clone());
    }
    Ok(credentials)
}

async fn chain(
    env: &AwsEnv,
    profile: Option<&str>,
    region: Option<&str>,
    cancel: &CancellationToken,
) -> Result<Credentials, String> {
    let profile_name = profile
        .or_else(|| env.get("AWS_PROFILE"))
        .unwrap_or("default")
        .to_owned();
    let profile_set = profile.is_some() || env.get("AWS_PROFILE").is_some();
    let mut last = String::new();
    for step in 0..5 {
        let result = match step {
            0 if profile_set => Err(ChainError::next(
                "AWS_PROFILE is set, skipping fromEnv provider.",
            )),
            0 => from_env(env),
            1 => from_ini(env, &profile_name, region, cancel).await,
            2 => from_token_file(env, region, cancel).await,
            3 => remote(env, cancel).await,
            _ => Err(ChainError::fatal(
                "Could not load credentials from any providers",
            )),
        };
        match result {
            Ok(credentials) => return Ok(credentials),
            Err(err) if err.next => last = err.message,
            Err(err) => return Err(err.message),
        }
    }
    Err(last)
}

/// `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN` and
/// `AWS_CREDENTIAL_EXPIRATION`.
fn from_env(env: &AwsEnv) -> Chained {
    match (
        env.get("AWS_ACCESS_KEY_ID"),
        env.get("AWS_SECRET_ACCESS_KEY"),
    ) {
        (Some(id), Some(secret)) => Ok(Credentials {
            access_key_id: id.to_owned(),
            secret_access_key: secret.to_owned(),
            session_token: env.get("AWS_SESSION_TOKEN").map(str::to_owned),
            expiration_ms: env.get("AWS_CREDENTIAL_EXPIRATION").and_then(parse_iso),
        }),
        _ => Err(ChainError::next(
            "Unable to find environment variable credentials.",
        )),
    }
}

async fn from_ini(
    env: &AwsEnv,
    profile: &str,
    region: Option<&str>,
    cancel: &CancellationToken,
) -> Chained {
    let profiles = ini::load(env).await;
    resolve_profile(env, &profiles, profile, region, Vec::new(), false, cancel).await
}

type Section = IndexMap<String, String>;

fn is_static(data: &Section) -> bool {
    data.contains_key("aws_access_key_id") && data.contains_key("aws_secret_access_key")
}

fn static_credentials(data: &Section) -> Credentials {
    Credentials {
        access_key_id: data["aws_access_key_id"].clone(),
        secret_access_key: data["aws_secret_access_key"].clone(),
        session_token: data.get("aws_session_token").cloned(),
        expiration_ms: None,
    }
}

fn is_assume_role(data: &Section) -> bool {
    data.contains_key("role_arn")
        && (data.contains_key("source_profile") != data.contains_key("credential_source"))
}

fn credential_source_without_role(data: &Section) -> bool {
    !data.contains_key("role_arn") && data.contains_key("credential_source")
}

/// The SDK's `resolveProfileData`: one profile's credentials, following
/// `source_profile` chains.
fn resolve_profile<'a>(
    env: &'a AwsEnv,
    profiles: &'a ini::Profiles,
    name: &'a str,
    region: Option<&'a str>,
    visited: Vec<String>,
    assume_role_source: bool,
    cancel: &'a CancellationToken,
) -> crate::auth::BoxFuture<'a, Chained> {
    Box::pin(async move {
        let empty = Section::new();
        let data = profiles.profiles.get(name).unwrap_or(&empty);
        if !visited.is_empty() && is_static(data) {
            return Ok(static_credentials(data));
        }
        if assume_role_source || is_assume_role(data) {
            return assume_role(env, profiles, name, data, region, visited, cancel).await;
        }
        if is_static(data) {
            return Ok(static_credentials(data));
        }
        if let (Some(token_file), Some(role)) =
            (data.get("web_identity_token_file"), data.get("role_arn"))
        {
            let token = read_token_file(token_file).await?;
            let session = data.get("role_session_name").cloned();
            return Ok(
                assume_role_with_web_identity(env, role, session, &token, region, cancel).await?,
            );
        }
        if let Some(command) = data.get("credential_process") {
            return Ok(credential_process(name, command, cancel).await?);
        }
        if [
            "sso_start_url",
            "sso_account_id",
            "sso_session",
            "sso_region",
            "sso_role_name",
        ]
        .iter()
        .any(|key| data.contains_key(*key))
        {
            return Ok(sso(env, profiles, name, data, cancel).await?);
        }
        if data.contains_key("login_session") {
            return Err(ChainError::fatal(format!(
                "Profile {name} uses console login credentials, which yapi cannot read yet. Run `aws configure export-credentials` or use another credential source."
            )));
        }
        Err(ChainError::next(format!(
            "Could not resolve credentials using profile: [{name}] in configuration/credentials file(s)."
        )))
    })
}

async fn assume_role(
    env: &AwsEnv,
    profiles: &ini::Profiles,
    name: &str,
    data: &Section,
    region: Option<&str>,
    visited: Vec<String>,
    cancel: &CancellationToken,
) -> Chained {
    let region = data.get("region").map(String::as_str).or(region);
    let source = if let Some(source) = data.get("source_profile") {
        if visited.contains(source) {
            return Err(ChainError::fatal(format!(
                "Detected a cycle attempting to resolve credentials for profile {name}. Profiles visited: {}",
                visited.join(", ")
            )));
        }
        let mut next = visited.clone();
        next.push(source.clone());
        let recursive = profiles
            .profiles
            .get(source)
            .is_some_and(credential_source_without_role);
        resolve_profile(env, profiles, source, region, next, recursive, cancel).await?
    } else {
        let source = data
            .get("credential_source")
            .map(String::as_str)
            .unwrap_or_default();
        match source {
            "EcsContainer" => container(env, cancel).await?,
            "Ec2InstanceMetadata" => instance_metadata(env, cancel).await?,
            "Environment" => from_env(env)?,
            other => {
                return Err(ChainError::fatal(format!(
                    "Unsupported credential source in profile {name}. Got {other}, expected EcsContainer or Ec2InstanceMetadata or Environment."
                )));
            }
        }
    };
    if credential_source_without_role(data) {
        return Ok(source);
    }
    if data.contains_key("mfa_serial") {
        return Err(ChainError::fatal(format!(
            "Profile {name} requires multi-factor authentication, but no MFA code callback was provided."
        )));
    }
    let session = data
        .get("role_session_name")
        .cloned()
        .unwrap_or_else(|| format!("aws-sdk-js-{}", now_ms()));
    let mut params = vec![
        ("Action", "AssumeRole".to_owned()),
        ("Version", "2011-06-15".to_owned()),
        ("RoleArn", data["role_arn"].clone()),
        ("RoleSessionName", session),
    ];
    if let Some(external) = data.get("external_id") {
        params.push(("ExternalId", external.clone()));
    }
    params.push((
        "DurationSeconds",
        data.get("duration_seconds")
            .cloned()
            .unwrap_or_else(|| "3600".into()),
    ));
    Ok(sts(env, params, Some(&source), region, cancel).await?)
}

async fn read_token_file(path: &str) -> Result<String, ChainError> {
    tokio::fs::read_to_string(path)
        .await
        .map(|text| text.trim().to_owned())
        .map_err(|err| {
            ChainError::fatal(format!(
                "Failed to read web identity token file {path}: {err}"
            ))
        })
}

/// `AWS_WEB_IDENTITY_TOKEN_FILE` with `AWS_ROLE_ARN`, as IRSA sets them.
async fn from_token_file(
    env: &AwsEnv,
    region: Option<&str>,
    cancel: &CancellationToken,
) -> Chained {
    let (Some(file), Some(role)) = (
        env.get("AWS_WEB_IDENTITY_TOKEN_FILE"),
        env.get("AWS_ROLE_ARN"),
    ) else {
        return Err(ChainError::next("Web identity configuration not specified"));
    };
    let token = read_token_file(file).await?;
    let session = env.get("AWS_ROLE_SESSION_NAME").map(str::to_owned);
    Ok(assume_role_with_web_identity(env, role, session, &token, region, cancel).await?)
}

async fn assume_role_with_web_identity(
    env: &AwsEnv,
    role: &str,
    session: Option<String>,
    token: &str,
    region: Option<&str>,
    cancel: &CancellationToken,
) -> Result<Credentials, String> {
    let params = vec![
        ("Action", "AssumeRoleWithWebIdentity".to_owned()),
        ("Version", "2011-06-15".to_owned()),
        ("RoleArn", role.to_owned()),
        (
            "RoleSessionName",
            session.unwrap_or_else(|| format!("aws-sdk-js-session-{}", now_ms())),
        ),
        ("WebIdentityToken", token.to_owned()),
    ];
    sts(env, params, None, region, cancel).await
}

/// The text of the first `<tag>` element.
fn xml_text(xml: &str, tag: &str) -> Option<String> {
    let start = xml.find(&format!("<{tag}>"))? + tag.len() + 2;
    let end = start + xml[start..].find(&format!("</{tag}>"))?;
    Some(
        xml[start..end]
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&"),
    )
}

/// The STS endpoint: `AWS_ENDPOINT_URL_STS`, else `AWS_ENDPOINT_URL`, else
/// the regional one.
fn sts_endpoint(env: &AwsEnv, region: &str) -> String {
    env.get("AWS_ENDPOINT_URL_STS")
        .or_else(|| env.get("AWS_ENDPOINT_URL"))
        .map_or_else(
            || {
                let suffix = if region.starts_with("cn-") { ".cn" } else { "" };
                format!("https://sts.{region}.amazonaws.com{suffix}")
            },
            str::to_owned,
        )
}

/// An STS query call, signed with `signer` when given.
async fn sts(
    env: &AwsEnv,
    params: Vec<(&str, String)>,
    signer: Option<&Credentials>,
    region: Option<&str>,
    cancel: &CancellationToken,
) -> Result<Credentials, String> {
    let region = region.unwrap_or("us-east-1");
    let endpoint = sts_endpoint(env, region);
    let pairs: Vec<(&str, &str)> = params.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let body = crate::auth::form(&pairs);
    let url = url::Url::parse(&endpoint).map_err(|err| err.to_string())?;
    let mut headers = vec![(
        "content-type".to_owned(),
        "application/x-www-form-urlencoded".to_owned(),
    )];
    if let Some(credentials) = signer {
        sigv4::sign_request(
            &url,
            &mut headers,
            body.as_bytes(),
            credentials,
            region,
            "sts",
        );
    }
    let mut request = crate::http::client().post(url.as_str()).body(body);
    for (name, value) in &headers {
        request = request.header(name.as_str(), value.as_str());
    }
    let response = send(request, cancel).await?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    if !(200..300).contains(&status) {
        let code = xml_text(&text, "Code").unwrap_or_else(|| "Unknown".into());
        let message = xml_text(&text, "Message").unwrap_or_else(|| format!("HTTP {status}"));
        return Err(format!("{code}: {message}"));
    }
    Ok(Credentials {
        access_key_id: xml_text(&text, "AccessKeyId").ok_or("STS returned no AccessKeyId")?,
        secret_access_key: xml_text(&text, "SecretAccessKey")
            .ok_or("STS returned no SecretAccessKey")?,
        session_token: xml_text(&text, "SessionToken"),
        expiration_ms: xml_text(&text, "Expiration").as_deref().and_then(parse_iso),
    })
}

async fn send(
    request: reqwest::RequestBuilder,
    cancel: &CancellationToken,
) -> Result<reqwest::Response, String> {
    crate::auth::send(request, cancel)
        .await
        .map_err(|err| err.to_string())
}

/// A profile's `credential_process`: run through the shell, its JSON output
/// parsed as version 1 credentials.
async fn credential_process(
    profile: &str,
    command: &str,
    cancel: &CancellationToken,
) -> Result<Credentials, String> {
    let run = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output();
    let output = tokio::select! {
        output = run => output.map_err(|err| err.to_string())?,
        () = cancel.cancelled() => return Err(crate::http::ABORTED_BEFORE_RESPONSE.into()),
    };
    if !output.status.success() {
        return Err(format!(
            "Command failed: {command}\n{}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let data: Value =
        serde_json::from_slice(String::from_utf8_lossy(&output.stdout).trim().as_bytes())
            .map_err(|_| format!("Profile {profile} credential_process returned invalid JSON."))?;
    if data["Version"].as_u64() != Some(1) {
        return Err(format!(
            "Profile {profile} credential_process did not return Version 1."
        ));
    }
    let (Some(id), Some(secret)) = (
        data["AccessKeyId"].as_str(),
        data["SecretAccessKey"].as_str(),
    ) else {
        return Err(format!(
            "Profile {profile} credential_process returned invalid credentials."
        ));
    };
    let expiration_ms = data["Expiration"].as_str().and_then(parse_iso);
    if expiration_ms.is_some_and(|expires| expires < now_ms()) {
        return Err(format!(
            "Profile {profile} credential_process returned expired credentials."
        ));
    }
    Ok(Credentials {
        access_key_id: id.to_owned(),
        secret_access_key: secret.to_owned(),
        session_token: data["SessionToken"].as_str().map(str::to_owned),
        expiration_ms,
    })
}

const SSO_REFRESH: &str =
    "To refresh this SSO session run aws sso login with the corresponding profile.";

/// IAM Identity Center: the cached `aws sso login` token exchanged for role
/// credentials.
async fn sso(
    env: &AwsEnv,
    profiles: &ini::Profiles,
    name: &str,
    data: &Section,
    cancel: &CancellationToken,
) -> Result<Credentials, String> {
    let session = data.get("sso_session");
    let session_data = session.and_then(|session| profiles.sso_sessions.get(session));
    let setting = |key: &str| {
        data.get(key)
            .or_else(|| session_data.and_then(|session| session.get(key)))
            .cloned()
    };
    let (Some(account), Some(role), Some(region)) = (
        setting("sso_account_id"),
        setting("sso_role_name"),
        setting("sso_region"),
    ) else {
        return Err(format!(
            "Profile is configured with invalid SSO credentials. Required parameters \"sso_account_id\", \"sso_region\", \"sso_role_name\", \"sso_start_url\". Got {}\nReference: https://docs.aws.amazon.com/cli/latest/userguide/cli-configure-sso.html",
            data.keys().cloned().collect::<Vec<_>>().join(", ")
        ));
    };
    let cache_id = match session {
        Some(session) => session.clone(),
        None => setting("sso_start_url").unwrap_or_default(),
    };
    let file = ini::home(env).join(".aws/sso/cache").join(format!(
        "{}.json",
        yapi_types::time::hex(
            aws_lc_rs::digest::digest(
                &aws_lc_rs::digest::SHA1_FOR_LEGACY_USE_ONLY,
                cache_id.as_bytes()
            )
            .as_ref()
        )
    ));
    let invalid =
        || format!("The SSO session associated with this profile is invalid. {SSO_REFRESH}");
    let token: Value = tokio::fs::read_to_string(&file)
        .await
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .ok_or_else(invalid)?;
    let access = token["accessToken"].as_str().ok_or_else(invalid)?;
    let expires = token["expiresAt"].as_str().and_then(parse_iso).unwrap_or(0);
    if expires <= now_ms() {
        return Err(format!(
            "The SSO session associated with this profile has expired. {SSO_REFRESH}"
        ));
    }
    let base = env
        .get("AWS_ENDPOINT_URL_SSO")
        .or_else(|| env.get("AWS_ENDPOINT_URL"))
        .map_or_else(
            || format!("https://portal.sso.{region}.amazonaws.com"),
            str::to_owned,
        );
    let mut url = url::Url::parse(&format!("{base}/federation/credentials"))
        .map_err(|err| err.to_string())?;
    url.query_pairs_mut()
        .append_pair("role_name", &role)
        .append_pair("account_id", &account);
    let request = crate::http::client()
        .get(url.as_str())
        .header("x-amz-sso_bearer_token", access);
    let response = send(request, cancel).await?;
    let status = response.status().as_u16();
    let body = crate::auth::json_body(response).await;
    if !(200..300).contains(&status) {
        return Err(body["message"].as_str().map_or_else(
            || format!("SSO GetRoleCredentials failed with status {status} for profile {name}"),
            str::to_owned,
        ));
    }
    let credentials = &body["roleCredentials"];
    match (
        credentials["accessKeyId"].as_str(),
        credentials["secretAccessKey"].as_str(),
        credentials["sessionToken"].as_str(),
        credentials["expiration"].as_u64(),
    ) {
        (Some(id), Some(secret), Some(token), Some(expiration)) => Ok(Credentials {
            access_key_id: id.to_owned(),
            secret_access_key: secret.to_owned(),
            session_token: Some(token.to_owned()),
            expiration_ms: Some(expiration),
        }),
        _ => Err("SSO returns an invalid temporary credential.".into()),
    }
}

/// Metadata services answer quickly or not at all.
const METADATA_TIMEOUT: Duration = Duration::from_secs(1);

/// Container credentials, else instance metadata unless disabled. Failures
/// leave the chain's final error.
async fn remote(env: &AwsEnv, cancel: &CancellationToken) -> Chained {
    if env.get("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI").is_some()
        || env.get("AWS_CONTAINER_CREDENTIALS_FULL_URI").is_some()
    {
        return container(env, cancel).await.map_err(ChainError::next);
    }
    if env
        .get("AWS_EC2_METADATA_DISABLED")
        .is_some_and(|value| value != "false")
    {
        return Err(ChainError::next(
            "EC2 Instance Metadata Service access disabled",
        ));
    }
    instance_metadata(env, cancel)
        .await
        .map_err(ChainError::next)
}

/// Credentials from a JSON metadata document.
fn metadata_credentials(body: &Value) -> Result<Credentials, String> {
    match (
        body["AccessKeyId"].as_str(),
        body["SecretAccessKey"].as_str(),
    ) {
        (Some(id), Some(secret)) => Ok(Credentials {
            access_key_id: id.to_owned(),
            secret_access_key: secret.to_owned(),
            session_token: body["Token"].as_str().map(str::to_owned),
            expiration_ms: body["Expiration"].as_str().and_then(parse_iso),
        }),
        _ => Err("Invalid response received from instance metadata service.".into()),
    }
}

/// ECS and EKS Pod Identity credentials.
async fn container(env: &AwsEnv, cancel: &CancellationToken) -> Result<Credentials, String> {
    let url = match (
        env.get("AWS_CONTAINER_CREDENTIALS_FULL_URI"),
        env.get("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI"),
    ) {
        (_, Some(relative)) => format!("http://169.254.170.2{relative}"),
        (Some(full), None) => full.to_owned(),
        (None, None) => {
            return Err("No container credentials endpoint is configured".into());
        }
    };
    let mut request = crate::http::client().get(&url).timeout(METADATA_TIMEOUT);
    let token = match env.get("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE") {
        Some(file) => tokio::fs::read_to_string(file)
            .await
            .ok()
            .map(|text| text.trim().to_owned()),
        None => env
            .get("AWS_CONTAINER_AUTHORIZATION_TOKEN")
            .map(str::to_owned),
    };
    if let Some(token) = token {
        request = request.header("authorization", token);
    }
    let response = send(request, cancel).await?;
    let body = crate::auth::json_body(response).await;
    metadata_credentials(&body)
}

/// EC2 instance metadata with an IMDSv2 session token.
async fn instance_metadata(
    env: &AwsEnv,
    cancel: &CancellationToken,
) -> Result<Credentials, String> {
    let base = env
        .get("AWS_EC2_METADATA_SERVICE_ENDPOINT")
        .unwrap_or("http://169.254.169.254")
        .trim_end_matches('/')
        .to_owned();
    let failed = |_| "Could not load credentials from the instance metadata service".to_owned();
    let token = send(
        crate::http::client()
            .put(format!("{base}/latest/api/token"))
            .header("x-aws-ec2-metadata-token-ttl-seconds", "21600")
            .timeout(METADATA_TIMEOUT),
        cancel,
    )
    .await
    .map_err(failed)?
    .text()
    .await
    .unwrap_or_default();
    let get = |path: String| {
        crate::http::client()
            .get(format!("{base}{path}"))
            .header("x-aws-ec2-metadata-token", token.clone())
            .timeout(METADATA_TIMEOUT)
    };
    let names = send(
        get("/latest/meta-data/iam/security-credentials/".into()),
        cancel,
    )
    .await
    .map_err(failed)?
    .text()
    .await
    .unwrap_or_default();
    let role = names
        .lines()
        .next()
        .filter(|role| !role.is_empty())
        .ok_or_else(|| "No instance profile is attached to this instance".to_owned())?;
    let response = send(
        get(format!("/latest/meta-data/iam/security-credentials/{role}")),
        cancel,
    )
    .await
    .map_err(failed)?;
    let body = crate::auth::json_body(response).await;
    metadata_credentials(&body)
}

/// The region of a profile in the config file, as the SDK's region chain
/// falls back to it.
pub async fn profile_region(env: &AwsEnv, profile: Option<&str>) -> Option<String> {
    let name = profile
        .or_else(|| env.get("AWS_PROFILE"))
        .unwrap_or("default");
    ini::load(env)
        .await
        .profiles
        .get(name)
        .and_then(|data| data.get("region"))
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("yapi-aws-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".aws")).unwrap();
        dir
    }

    #[tokio::test]
    async fn resolves_profiles_without_the_process_environment() {
        let home = scratch("profiles");
        std::fs::write(
            home.join(".aws/credentials"),
            "[default]\naws_access_key_id = AKIDDEFAULT\naws_secret_access_key = s1\n\n[static]\naws_access_key_id = AKIDSTATIC\naws_secret_access_key = s2\naws_session_token = t2\n",
        )
        .unwrap();
        std::fs::write(
            home.join(".aws/config"),
            "[profile proc]\ncredential_process = printf '{\"Version\":1,\"AccessKeyId\":\"AKIDPROC\",\"SecretAccessKey\":\"s3\"}'\n\n[profile loop]\nrole_arn = arn:aws:iam::1:role/r\nsource_profile = loop\n\n[profile mfa]\nrole_arn = arn:aws:iam::1:role/r\nsource_profile = static\nmfa_serial = arn:aws:iam::1:mfa/me\n\n[profile region-only]\nregion = eu-west-3\n",
        )
        .unwrap();
        let home_str = home.to_str().unwrap();
        let env = AwsEnv::from_vars([("HOME", home_str), ("AWS_EC2_METADATA_DISABLED", "true")]);
        let cancel = CancellationToken::new();
        let id = |profile: Option<&'static str>| {
            let env = env.clone();
            let cancel = cancel.clone();
            async move {
                chain(&env, profile, None, &cancel)
                    .await
                    .map(|credentials| credentials.access_key_id)
            }
        };
        assert_eq!(id(None).await.as_deref(), Ok("AKIDDEFAULT"));
        assert_eq!(id(Some("static")).await.as_deref(), Ok("AKIDSTATIC"));
        assert_eq!(id(Some("proc")).await.as_deref(), Ok("AKIDPROC"));
        assert!(
            id(Some("loop"))
                .await
                .unwrap_err()
                .contains("Detected a cycle")
        );
        assert!(
            id(Some("mfa"))
                .await
                .unwrap_err()
                .contains("requires multi-factor authentication")
        );
        assert_eq!(
            id(Some("missing")).await.unwrap_err(),
            "Could not load credentials from any providers"
        );
        // Environment keys win only while no profile is chosen.
        let keyed = AwsEnv::from_vars([
            ("HOME", home_str),
            ("AWS_ACCESS_KEY_ID", "AKIDENV"),
            ("AWS_SECRET_ACCESS_KEY", "s4"),
        ]);
        assert_eq!(
            chain(&keyed, None, None, &cancel)
                .await
                .unwrap()
                .access_key_id,
            "AKIDENV"
        );
        assert_eq!(
            chain(&keyed, Some("static"), None, &cancel)
                .await
                .unwrap()
                .access_key_id,
            "AKIDSTATIC"
        );
        assert_eq!(
            profile_region(&env, Some("region-only")).await.as_deref(),
            Some("eu-west-3")
        );
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn reads_sts_xml() {
        let xml = "<AssumeRoleResponse><AssumeRoleResult><Credentials><AccessKeyId>ASIA</AccessKeyId><SecretAccessKey>s&amp;k</SecretAccessKey><SessionToken>tok</SessionToken><Expiration>2026-10-04T20:00:00Z</Expiration></Credentials></AssumeRoleResult></AssumeRoleResponse>";
        assert_eq!(xml_text(xml, "SecretAccessKey").as_deref(), Some("s&k"));
        assert_eq!(xml_text(xml, "Missing"), None);
    }
}
