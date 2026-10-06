//! OAuth for MCP servers: protected resource and authorization server
//! discovery, dynamic client registration, the authorization code flow with
//! PKCE, and refresh. Port of `oauth/` in pi-mcp `v1.0.0`, which adapts the
//! MCP TypeScript SDK, limited to what pi's coding agent uses.
//!
//! Metadata, client registrations and tokens are JSON objects kept as read,
//! so the state pi and yapi share in `mcp-auth.json` keeps its fields and
//! their order.

use std::sync::LazyLock;
use std::time::Duration;

use base64::Engine as _;
use regex_lite::Regex;
use reqwest::Url;
use serde_json::{Map, Value};

use super::client::LATEST_PROTOCOL_VERSION;
use super::sign_in::ServerStore;

/// A JSON object as pi spreads and stores it.
pub type Object = Map<String, Value>;

/// Why an OAuth step failed. Messages match pi-mcp's.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum OAuthError {
    /// An OAuth error response; pi's `OAuthError`.
    #[error("{message}")]
    Response {
        /// The `error` code.
        code: String,
        /// `error_description`, else the code.
        message: String,
    },
    /// A token request would send credentials over plain HTTP.
    #[error("Refusing to send OAuth credentials to non-HTTPS endpoint {0}")]
    InsecureEndpoint(String),
    /// A request got no response, as `fetch` failing.
    #[error("{0}")]
    Network(String),
    /// Anything else, with pi's message.
    #[error("{0}")]
    Other(String),
}

fn other(message: impl Into<String>) -> OAuthError {
    OAuthError::Other(message.into())
}

/// pi's `OAuthIssuerMismatchError`.
fn issuer_mismatch(expected: &str, received: Option<&str>) -> OAuthError {
    let quote = |text: &str| yapi_types::json::stringify(&Value::from(text));
    other(format!(
        "OAuth issuer mismatch: expected {}, received {}",
        quote(expected),
        received.map_or_else(|| "none".to_owned(), quote)
    ))
}

// ---------------------------------------------------------------------------
// Parsing (types.ts)
// ---------------------------------------------------------------------------

fn object<'a>(value: &'a Value, name: &str) -> Result<&'a Object, OAuthError> {
    value
        .as_object()
        .ok_or_else(|| other(format!("Invalid {name}")))
}

/// `null` and `""` count as absent: servers send them for fields they have
/// no value for.
fn absent(value: Option<&Value>) -> bool {
    matches!(value, None | Some(Value::Null)) || value.and_then(Value::as_str) == Some("")
}

fn required_string(value: Option<&Value>, name: &str) -> Result<String, OAuthError> {
    value
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| other(format!("Invalid {name}")))
}

fn optional_string(value: Option<&Value>, name: &str) -> Result<Option<Value>, OAuthError> {
    if absent(value) {
        return Ok(None);
    }
    required_string(value, name).map(|text| Some(Value::String(text)))
}

fn optional_strings(value: Option<&Value>, name: &str) -> Result<Option<Vec<Value>>, OAuthError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) if items.iter().all(Value::is_string) => Ok(Some(items.clone())),
        Some(_) => Err(other(format!("Invalid {name}"))),
    }
}

fn safe_url(value: Option<&Value>, name: &str) -> Result<String, OAuthError> {
    let text = required_string(value, name)?;
    match Url::parse(&text) {
        Ok(url) if !["javascript", "data", "vbscript"].contains(&url.scheme()) => Ok(text),
        _ => Err(other(format!("Invalid {name}"))),
    }
}

fn optional_url(value: Option<&Value>, name: &str) -> Result<Option<Value>, OAuthError> {
    if absent(value) {
        return Ok(None);
    }
    safe_url(value, name).map(|text| Some(Value::String(text)))
}

/// `compact({...input, key: value, ...})`: each field replaces its namesake in
/// place or is appended; `None` removes it.
fn spread<'a>(
    input: &Object,
    fields: impl IntoIterator<Item = (&'a str, Option<Value>)>,
) -> Object {
    let mut out = input.clone();
    for (key, value) in fields {
        match value {
            Some(value) => {
                out.insert(key.to_owned(), value);
            }
            None => {
                out.shift_remove(key);
            }
        }
    }
    out
}

fn string_field(value: Option<&Value>, name: &str) -> Result<Option<Value>, OAuthError> {
    required_string(value, name).map(|text| Some(Value::String(text)))
}

fn url_field(value: Option<&Value>, name: &str) -> Result<Option<Value>, OAuthError> {
    safe_url(value, name).map(|text| Some(Value::String(text)))
}

fn strings_field(value: Option<&Value>, name: &str) -> Result<Option<Value>, OAuthError> {
    optional_strings(value, name).map(|items| items.map(Value::Array))
}

fn boolean(value: Option<&Value>) -> Option<Value> {
    value.filter(|value| value.is_boolean()).cloned()
}

fn parse_protected_resource_metadata(value: &Value) -> Result<Object, OAuthError> {
    let input = object(value, "OAuth protected resource metadata")?;
    let resource = url_field(
        input.get("resource"),
        "OAuth protected resource metadata resource",
    )?;
    let servers = optional_strings(input.get("authorization_servers"), "authorization_servers")?
        .map(|servers| {
            servers
                .iter()
                .map(|url| safe_url(Some(url), "authorization server URL").map(Value::String))
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?;
    Ok(spread(
        input,
        [
            ("resource", resource),
            ("authorization_servers", servers.map(Value::Array)),
            (
                "scopes_supported",
                strings_field(input.get("scopes_supported"), "scopes_supported")?,
            ),
        ],
    ))
}

fn parse_authorization_server_metadata(value: &Value) -> Result<Object, OAuthError> {
    let input = object(value, "authorization server metadata")?;
    let get = |key: &str| input.get(key);
    let list = |key: &str| strings_field(get(key), key);
    let response_types = list("response_types_supported")?
        .ok_or_else(|| other("Invalid response_types_supported"))?;
    let fields = [
        (
            "issuer",
            url_field(get("issuer"), "authorization server issuer")?,
        ),
        (
            "authorization_endpoint",
            url_field(get("authorization_endpoint"), "authorization endpoint")?,
        ),
        (
            "token_endpoint",
            url_field(get("token_endpoint"), "token endpoint")?,
        ),
        (
            "registration_endpoint",
            optional_url(get("registration_endpoint"), "registration endpoint")?,
        ),
        ("scopes_supported", list("scopes_supported")?),
        ("response_types_supported", Some(response_types)),
        ("grant_types_supported", list("grant_types_supported")?),
        (
            "token_endpoint_auth_methods_supported",
            list("token_endpoint_auth_methods_supported")?,
        ),
        (
            "code_challenge_methods_supported",
            list("code_challenge_methods_supported")?,
        ),
        (
            "client_id_metadata_document_supported",
            boolean(get("client_id_metadata_document_supported")),
        ),
        (
            "authorization_response_iss_parameter_supported",
            boolean(get("authorization_response_iss_parameter_supported")),
        ),
    ];
    Ok(spread(input, fields))
}

/// `Number(value)` for `expires_in`; `None` when it is not a number.
fn js_number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => Some(yapi_types::json::js_number(text)),
        Value::Bool(flag) => Some(f64::from(u8::from(*flag))),
        _ => None,
    }
}

fn parse_tokens(value: &Value) -> Result<Object, OAuthError> {
    let input = object(value, "OAuth token response")?;
    // `Number(null)` is 0, which would mark the token as expired at once.
    let expires_in = match input.get("expires_in") {
        value if absent(value) => None,
        value => match value.and_then(js_number) {
            Some(seconds) if seconds.is_finite() => Some(yapi_types::json::number(seconds)),
            _ => return Err(other("Invalid expires_in")),
        },
    };
    let get = |key: &str| optional_string(input.get(key), key);
    let fields = [
        (
            "access_token",
            string_field(input.get("access_token"), "access_token")?,
        ),
        (
            "token_type",
            string_field(input.get("token_type"), "token_type")?,
        ),
        ("expires_in", expires_in),
        ("scope", get("scope")?),
        ("refresh_token", get("refresh_token")?),
        ("id_token", get("id_token")?),
    ];
    Ok(spread(&Object::new(), fields))
}

fn parse_client_information(value: &Value) -> Result<Object, OAuthError> {
    let input = object(value, "OAuth client registration response")?;
    let number = |key: &str| input.get(key).filter(|value| value.is_number()).cloned();
    let fields = [
        (
            "client_id",
            string_field(input.get("client_id"), "client_id")?,
        ),
        (
            "client_secret",
            optional_string(input.get("client_secret"), "client_secret")?,
        ),
        ("client_id_issued_at", number("client_id_issued_at")),
        (
            "client_secret_expires_at",
            number("client_secret_expires_at"),
        ),
        (
            "redirect_uris",
            Some(
                strings_field(input.get("redirect_uris"), "redirect_uris")?
                    .unwrap_or_else(|| Value::Array(Vec::new())),
            ),
        ),
    ];
    Ok(spread(input, fields))
}

/// The string field `key` of an object.
pub(super) fn text<'a>(object: &'a Object, key: &str) -> Option<&'a str> {
    object.get(key).and_then(Value::as_str)
}

/// The non-empty string field `key`, as JavaScript's truthiness reads it.
fn truthy<'a>(object: &'a Object, key: &str) -> Option<&'a str> {
    text(object, key).filter(|text| !text.is_empty())
}

fn list<'a>(object: &'a Object, key: &str) -> Option<Vec<&'a str>> {
    object
        .get(key)
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).collect())
}

// ---------------------------------------------------------------------------
// Discovery (discovery.ts)
// ---------------------------------------------------------------------------

/// A `WWW-Authenticate` challenge; pi's `OAuthChallenge`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Challenge {
    /// `resource_metadata`, when it is a URL.
    pub resource_metadata_url: Option<String>,
    /// `scope`.
    pub scope: Option<String>,
    /// `error`, such as `insufficient_scope`.
    pub error: Option<String>,
    /// `error_description`.
    pub error_description: Option<String>,
}

fn field(header: &str, name: &str) -> Option<String> {
    let pattern = format!(r#"(?i)(?:^|[,\s]){name}=(?:"([^"]*)"|([^\s,]+))"#);
    let captures = Regex::new(&pattern).ok()?.captures(header)?;
    // An empty value carries no information, so it counts as absent.
    captures
        .get(1)
        .or_else(|| captures.get(2))
        .map(|value| value.as_str().to_owned())
        .filter(|value| !value.is_empty())
}

/// pi's `parseWwwAuthenticate`: the fields of a Bearer or DPoP challenge.
pub fn parse_www_authenticate(header: Option<&str>) -> Challenge {
    let Some(header) = header.filter(|header| !header.is_empty()) else {
        return Challenge::default();
    };
    let scheme = header
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if scheme != "bearer" && scheme != "dpop" {
        return Challenge::default();
    }
    Challenge {
        resource_metadata_url: field(header, "resource_metadata")
            .and_then(|url| Url::parse(&url).ok())
            .map(String::from),
        scope: field(header, "scope"),
        error: field(header, "error"),
        error_description: field(header, "error_description"),
    }
}

/// Whether a 403 response asks for more scope (step-up authorization).
pub fn is_insufficient_scope(header: Option<&str>) -> bool {
    static PATTERN: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r#"(?i)(?:^|[\s,])error="?insufficient_scope"?"#).ok());
    header.is_some_and(|header| {
        PATTERN
            .as_ref()
            .is_some_and(|pattern| pattern.is_match(header))
    })
}

/// How requests of one flow are sent.
#[derive(Clone, Copy, Debug, Default)]
pub struct Fetch {
    /// Bounds each request.
    pub timeout: Option<Duration>,
}

impl Fetch {
    async fn send(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, OAuthError> {
        let request = match self.timeout {
            Some(timeout) => request.timeout(timeout),
            None => request,
        };
        request.send().await.map_err(|error| {
            if error.is_timeout() {
                other("The operation was aborted due to timeout")
            } else {
                // Node's `fetch` reports every network failure this way.
                OAuthError::Network("fetch failed".into())
            }
        })
    }

    async fn metadata(&self, url: &Url) -> Result<reqwest::Response, OAuthError> {
        self.send(
            yapi_ai::http::client()
                .get(url.clone())
                .header("Accept", "application/json")
                .header("MCP-Protocol-Version", LATEST_PROTOCOL_VERSION),
        )
        .await
    }
}

async fn json(response: reqwest::Response) -> Result<Value, OAuthError> {
    let bytes = response
        .bytes()
        .await
        .map_err(|_| OAuthError::Network("fetch failed".into()))?;
    serde_json::from_slice(&bytes).map_err(|error| other(error.to_string()))
}

/// 4xx and 502 mean "not here", so discovery tries the next candidate.
fn is_discovery_miss(status: u16) -> bool {
    (400..500).contains(&status) || status == 502
}

/// The path suffix for `/.well-known/<kind><path>`; empty for the root path.
fn path_suffix(path: &str) -> &str {
    path.strip_suffix('/').unwrap_or(path)
}

fn parse_url(text: &str) -> Result<Url, OAuthError> {
    Url::parse(text).map_err(|_| other(format!("Invalid URL: {text}")))
}

fn join(base: &Url, path: &str) -> Result<Url, OAuthError> {
    base.join(path)
        .map_err(|_| other(format!("Invalid URL: {path}")))
}

async fn discover_protected_resource_metadata(
    server_url: &str,
    resource_metadata_url: Option<&str>,
    fetch: Fetch,
) -> Result<Object, OAuthError> {
    let server = parse_url(server_url)?;
    let url = match resource_metadata_url {
        Some(url) => parse_url(url)?,
        None => join(
            &server,
            &format!(
                "/.well-known/oauth-protected-resource{}",
                path_suffix(server.path())
            ),
        )?,
    };
    let mut response = fetch.metadata(&url).await?;
    if resource_metadata_url.is_none()
        && server.path() != "/"
        && is_discovery_miss(response.status().as_u16())
    {
        response = fetch
            .metadata(&join(&server, "/.well-known/oauth-protected-resource")?)
            .await?;
    }
    if !response.status().is_success() {
        return Err(other(format!(
            "HTTP {} loading OAuth protected resource metadata",
            response.status().as_u16()
        )));
    }
    parse_protected_resource_metadata(&json(response).await?)
}

/// pi's `buildAuthorizationServerDiscoveryUrls`: RFC 8414, then OpenID
/// Connect discovery with the path inserted and appended.
fn discovery_urls(authorization_server_url: &str) -> Result<Vec<Url>, OAuthError> {
    let issuer = parse_url(authorization_server_url)?;
    let path = path_suffix(issuer.path());
    let mut urls = vec![
        join(
            &issuer,
            &format!("/.well-known/oauth-authorization-server{path}"),
        )?,
        join(&issuer, &format!("/.well-known/openid-configuration{path}"))?,
    ];
    if !path.is_empty() {
        urls.push(join(
            &issuer,
            &format!("{path}/.well-known/openid-configuration"),
        )?);
    }
    Ok(urls)
}

async fn discover_authorization_server_metadata(
    authorization_server_url: &str,
    fetch: Fetch,
) -> Result<Option<Object>, OAuthError> {
    for url in discovery_urls(authorization_server_url)? {
        let response = fetch.metadata(&url).await?;
        let status = response.status().as_u16();
        if !response.status().is_success() {
            if is_discovery_miss(status) {
                continue;
            }
            return Err(other(format!(
                "HTTP {status} loading authorization server metadata from {url}"
            )));
        }
        let metadata = parse_authorization_server_metadata(&json(response).await?)?;
        let issuer = text(&metadata, "issuer").unwrap_or_default();
        // URL parsing adds a trailing slash to bare origins, so compare without one.
        let trim = |value: &str| value.strip_suffix('/').unwrap_or(value).to_owned();
        if trim(issuer) != trim(authorization_server_url) {
            return Err(issuer_mismatch(authorization_server_url, Some(issuer)));
        }
        return Ok(Some(metadata));
    }
    Ok(None)
}

/// pi's `OAuthServerInfo` as the discovery state stores it:
/// `authorizationServerUrl`, `authorizationServerMetadata`,
/// `resourceMetadata`.
fn server_info(url: String, metadata: Option<Object>, resource: Option<Object>) -> Object {
    spread(
        &Object::new(),
        [
            ("authorizationServerUrl", Some(Value::String(url))),
            ("authorizationServerMetadata", metadata.map(Value::Object)),
            ("resourceMetadata", resource.map(Value::Object)),
        ],
    )
}

async fn discover_server_info(
    server_url: &str,
    resource_metadata_url: Option<&str>,
    metadata_url: Option<&Url>,
    fetch: Fetch,
) -> Result<Object, OAuthError> {
    let resource = match discover_protected_resource_metadata(
        server_url,
        resource_metadata_url,
        fetch,
    )
    .await
    {
        Ok(resource) => Some(resource),
        Err(OAuthError::Network(message)) => return Err(OAuthError::Network(message)),
        Err(_) => None,
    };
    if let Some(url) = metadata_url {
        let response = fetch.metadata(url).await?;
        if !response.status().is_success() {
            return Err(other(format!(
                "HTTP {} loading authorization server metadata from {url}",
                response.status().as_u16()
            )));
        }
        let metadata = parse_authorization_server_metadata(&json(response).await?)?;
        let issuer = text(&metadata, "issuer").unwrap_or_default().to_owned();
        return Ok(server_info(issuer, Some(metadata), resource));
    }
    let authorization_server = match resource
        .as_ref()
        .and_then(|resource| list(resource, "authorization_servers"))
        .and_then(|servers| servers.first().map(|server| (*server).to_owned()))
    {
        Some(server) => server,
        None => join(&parse_url(server_url)?, "/")?.to_string(),
    };
    let metadata = discover_authorization_server_metadata(&authorization_server, fetch).await?;
    Ok(server_info(authorization_server, metadata, resource))
}

/// pi's `selectResource`: the protected resource for the server, which must
/// contain its URL.
fn select_resource(
    server_url: &str,
    metadata: Option<&Object>,
) -> Result<Option<String>, OAuthError> {
    let Some(metadata) = metadata else {
        return Ok(None);
    };
    let resource = text(metadata, "resource").unwrap_or_default();
    let mut requested = parse_url(server_url)?;
    requested.set_fragment(None);
    let configured = parse_url(resource)?;
    let with_slash = |path: &str| {
        if path.ends_with('/') {
            path.to_owned()
        } else {
            format!("{path}/")
        }
    };
    if requested.origin() != configured.origin()
        || !with_slash(requested.path()).starts_with(&with_slash(configured.path()))
    {
        return Err(other(format!(
            "Protected resource {resource} does not match MCP server {requested}"
        )));
    }
    Ok(Some(resource.to_owned()))
}

// ---------------------------------------------------------------------------
// Flow (flow.ts)
// ---------------------------------------------------------------------------

fn is_loopback(url: &Url) -> bool {
    matches!(
        url.host_str(),
        Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
    )
}

/// The URL, unless it would send credentials over plain HTTP to another host.
pub fn secure_endpoint(value: &str) -> Result<Url, OAuthError> {
    let url = parse_url(value)?;
    if url.scheme() != "https" && !is_loopback(&url) {
        return Err(OAuthError::InsecureEndpoint(url.to_string()));
    }
    Ok(url)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ClientAuth {
    Basic,
    Post,
    None,
}

fn select_client_auth(information: &Object, supported: &[&str]) -> ClientAuth {
    let secret = truthy(information, "client_secret").is_some();
    let parse = |method: &str| match method {
        "client_secret_basic" => Some(ClientAuth::Basic),
        "client_secret_post" => Some(ClientAuth::Post),
        "none" => Some(ClientAuth::None),
        _ => None,
    };
    if let Some(hinted) = truthy(information, "token_endpoint_auth_method")
        && let Some(method) = parse(hinted)
        && (supported.is_empty() || supported.contains(&hinted))
    {
        return method;
    }
    if supported.is_empty() {
        return if secret {
            ClientAuth::Basic
        } else {
            ClientAuth::None
        };
    }
    if secret && supported.contains(&"client_secret_basic") {
        return ClientAuth::Basic;
    }
    if secret && supported.contains(&"client_secret_post") {
        return ClientAuth::Post;
    }
    if supported.contains(&"none") {
        return ClientAuth::None;
    }
    if secret {
        ClientAuth::Post
    } else {
        ClientAuth::None
    }
}

/// `URLSearchParams.set`: replaces every pair named `key` with one.
fn set(params: &mut Vec<(String, String)>, key: &str, value: &str) {
    params.retain(|(name, _)| name != key);
    params.push((key.to_owned(), value.to_owned()));
}

/// Where a token request goes and how the client authenticates.
struct TokenRequest<'a> {
    authorization_server_url: &'a str,
    metadata: Option<&'a Object>,
    client: &'a Object,
    resource: Option<&'a str>,
    fetch: Fetch,
}

impl TokenRequest<'_> {
    async fn send(&self, mut params: Vec<(String, String)>) -> Result<Object, OAuthError> {
        let endpoint = match self
            .metadata
            .and_then(|metadata| text(metadata, "token_endpoint"))
        {
            Some(endpoint) => endpoint.to_owned(),
            None => join(&parse_url(self.authorization_server_url)?, "/token")?.to_string(),
        };
        let url = secure_endpoint(&endpoint)?;
        if let Some(resource) = self.resource {
            set(&mut params, "resource", resource);
        }
        let supported = self
            .metadata
            .and_then(|metadata| list(metadata, "token_endpoint_auth_methods_supported"))
            .unwrap_or_default();
        let client_id = text(self.client, "client_id").unwrap_or_default();
        let secret = truthy(self.client, "client_secret");
        let mut basic = None;
        match select_client_auth(self.client, &supported) {
            ClientAuth::Basic => {
                let secret =
                    secret.ok_or_else(|| other("client_secret_basic requires a client secret"))?;
                let credentials = base64::engine::general_purpose::STANDARD
                    .encode(format!("{client_id}:{secret}"));
                basic = Some(format!("Basic {credentials}"));
            }
            method => {
                set(&mut params, "client_id", client_id);
                if method == ClientAuth::Post
                    && let Some(secret) = secret
                {
                    set(&mut params, "client_secret", secret);
                }
            }
        }
        let fields: Vec<(&str, &str)> = params
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        let mut request = yapi_ai::auth::post_form(url, &fields);
        if let Some(basic) = basic {
            request = request.header("Authorization", basic);
        }
        let response = self.fetch.send(request).await?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|_| OAuthError::Network("fetch failed".into()))?;
        let value: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        // Servers may report OAuth errors with any status.
        if let Some(code) = value.get("error").and_then(Value::as_str) {
            let message = value
                .get("error_description")
                .and_then(Value::as_str)
                .unwrap_or(code);
            return Err(OAuthError::Response {
                code: code.to_owned(),
                message: if message.is_empty() { code } else { message }.to_owned(),
            });
        }
        if !status.is_success() {
            return Err(OAuthError::Response {
                code: "server_error".into(),
                message: format!("HTTP {}: {body}", status.as_u16()),
            });
        }
        parse_tokens(&value)
    }
}

async fn register_client(
    authorization_server_url: &str,
    metadata: Option<&Object>,
    client_metadata: &Object,
    scope: Option<&str>,
    fetch: Fetch,
) -> Result<Object, OAuthError> {
    let endpoint = metadata.and_then(|metadata| text(metadata, "registration_endpoint"));
    if metadata.is_some() && endpoint.is_none() {
        return Err(other(
            "Authorization server does not support dynamic client registration",
        ));
    }
    let url = match endpoint {
        Some(endpoint) => parse_url(endpoint)?,
        None => join(&parse_url(authorization_server_url)?, "/register")?,
    };
    let mut body = client_metadata.clone();
    if let Some(scope) = scope {
        body.insert("scope".into(), Value::String(scope.to_owned()));
    }
    let response = fetch
        .send(
            yapi_ai::http::client()
                .post(url)
                .header("Accept", "application/json")
                .header("content-type", "application/json")
                .body(yapi_types::json::stringify(&body)),
        )
        .await?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(other(format!(
            "OAuth dynamic client registration failed with status {}: {body}",
            status.as_u16()
        )));
    }
    parse_client_information(&json(response).await?)
}

/// A response without `scope` grants the requested scope (RFC 6749).
fn with_scope(mut tokens: Object, scope: Option<&str>) -> Object {
    if !tokens.contains_key("scope")
        && let Some(scope) = scope.filter(|scope| !scope.is_empty())
    {
        tokens.insert("scope".into(), Value::String(scope.to_owned()));
    }
    tokens
}

/// What one run of the flow needs; pi's `OAuthFlowOptions`.
#[derive(Clone, Debug, Default)]
pub struct FlowOptions {
    /// The MCP server.
    pub server_url: String,
    /// The code an authorization response delivered.
    pub authorization_code: Option<String>,
    /// The `iss` parameter of that response (RFC 9207).
    pub iss: Option<String>,
    /// Scopes to request.
    pub scope: Option<String>,
    /// `resource_metadata` of the server's challenge.
    pub resource_metadata_url: Option<String>,
    /// Authorization server metadata to use instead of discovery.
    pub authorization_server_metadata_url: Option<String>,
    /// Go straight to authorization instead of refreshing stored tokens.
    pub skip_refresh: bool,
    /// How requests are sent.
    pub fetch: Fetch,
}

/// The outcome of [`authorize`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FlowResult {
    /// Tokens are stored.
    Authorized,
    /// The user has to open this authorization URL.
    Redirect(String),
}

/// What [`Provider::invalidate`] forgets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Invalidate {
    All,
    Tokens,
}

/// pi's `McpOAuthProvider`: the OAuth state of one exact server URL in a
/// [`ServerStore`].
pub struct Provider<'a> {
    server_url: String,
    redirect_url: String,
    client_metadata: Object,
    configured_client: Option<Object>,
    store: &'a ServerStore,
}

impl<'a> Provider<'a> {
    /// A provider for `server_url` that redirects to `redirect_url`;
    /// `client_id` and `client_secret` name a pre-registered client.
    pub fn new(
        server_url: &str,
        redirect_url: &str,
        client_name: &str,
        client_id: Option<&str>,
        client_secret: Option<&str>,
        store: &'a ServerStore,
    ) -> Result<Provider<'a>, OAuthError> {
        let mut client_metadata = Object::new();
        client_metadata.insert("client_name".into(), Value::from(client_name));
        client_metadata.insert("redirect_uris".into(), Value::from(vec![redirect_url]));
        client_metadata.insert(
            "grant_types".into(),
            Value::from(vec!["authorization_code", "refresh_token"]),
        );
        client_metadata.insert("response_types".into(), Value::from(vec!["code"]));
        client_metadata.insert(
            "token_endpoint_auth_method".into(),
            Value::from(if client_secret.is_some_and(|secret| !secret.is_empty()) {
                "client_secret_post"
            } else {
                "none"
            }),
        );
        let configured_client = client_id.filter(|id| !id.is_empty()).map(|id| {
            let mut client = Object::new();
            client.insert("client_id".into(), Value::from(id));
            if let Some(secret) = client_secret.filter(|secret| !secret.is_empty()) {
                client.insert("client_secret".into(), Value::from(secret));
            }
            client
        });
        Ok(Provider {
            server_url: parse_url(server_url)?.to_string(),
            redirect_url: redirect_url.to_owned(),
            client_metadata,
            configured_client,
            store,
        })
    }

    /// Stored state for another server URL is ignored, so credentials never
    /// leak across servers.
    async fn load(&self) -> Result<Object, OAuthError> {
        let state = self
            .store
            .load()
            .await
            .map_err(|error| other(error.to_string()))?;
        Ok(match state {
            Some(state) if text(&state, "serverUrl") == Some(self.server_url.as_str()) => state,
            _ => {
                let mut state = Object::new();
                state.insert("serverUrl".into(), Value::String(self.server_url.clone()));
                state
            }
        })
    }

    async fn update(&self, change: impl FnOnce(&mut Object)) -> Result<(), OAuthError> {
        let mut state = self.load().await?;
        change(&mut state);
        self.store
            .save(state)
            .await
            .map_err(|error| other(error.to_string()))
    }

    /// The `state` parameter, created once per sign-in.
    pub async fn state(&self) -> Result<String, OAuthError> {
        if let Some(state) = truthy(&self.load().await?, "oauthState") {
            return Ok(state.to_owned());
        }
        let state = yapi_types::time::random_hex(32);
        let value = state.clone();
        self.update(|stored| {
            stored.insert("oauthState".into(), Value::String(value));
        })
        .await?;
        Ok(state)
    }

    /// The stored object field `key`.
    async fn stored(&self, key: &str) -> Result<Option<Object>, OAuthError> {
        Ok(self
            .load()
            .await?
            .get(key)
            .and_then(Value::as_object)
            .cloned())
    }

    async fn client_information(&self) -> Result<Option<Object>, OAuthError> {
        match &self.configured_client {
            Some(client) => Ok(Some(client.clone())),
            None => self.stored("clientInformation").await,
        }
    }

    async fn save_client_information(&self, information: Object) -> Result<(), OAuthError> {
        if self.configured_client.is_some() {
            return Ok(());
        }
        self.update(|state| {
            state.insert("clientInformation".into(), Value::Object(information));
        })
        .await
    }

    async fn save_tokens(&self, tokens: Object) -> Result<(), OAuthError> {
        let expires_at = tokens
            .get("expires_in")
            .and_then(Value::as_f64)
            .map(|seconds| {
                yapi_types::json::number(yapi_types::time::now_ms() as f64 + seconds * 1000.0)
            });
        self.update(|state| {
            state.insert("tokens".into(), Value::Object(tokens));
            *state = spread(state, [("tokensExpireAt", expires_at)]);
        })
        .await
    }

    async fn code_verifier(&self) -> Result<String, OAuthError> {
        truthy(&self.load().await?, "codeVerifier")
            .map(str::to_owned)
            .ok_or_else(|| other("No OAuth PKCE code verifier is stored"))
    }

    async fn invalidate(&self, kind: Invalidate) -> Result<(), OAuthError> {
        self.update(|state| {
            let keys: &[&str] = match kind {
                Invalidate::All => &[
                    "clientInformation",
                    "tokens",
                    "tokensExpireAt",
                    "codeVerifier",
                    "discovery",
                    "oauthState",
                ],
                Invalidate::Tokens => &["tokens", "tokensExpireAt"],
            };
            for key in keys {
                state.shift_remove(*key);
            }
        })
        .await
    }
}

fn start_authorization(
    authorization_server_url: &str,
    metadata: Option<&Object>,
    client: &Object,
    redirect_url: &str,
    scope: Option<&str>,
    state: &str,
    resource: Option<&str>,
) -> Result<(Url, String), OAuthError> {
    if let Some(metadata) = metadata {
        if !list(metadata, "response_types_supported")
            .unwrap_or_default()
            .contains(&"code")
        {
            return Err(other(
                "Authorization server does not support authorization codes",
            ));
        }
        if list(metadata, "code_challenge_methods_supported")
            .is_some_and(|methods| !methods.contains(&"S256"))
        {
            return Err(other("Authorization server does not support PKCE S256"));
        }
    }
    let mut url = match metadata.and_then(|metadata| text(metadata, "authorization_endpoint")) {
        Some(endpoint) => parse_url(endpoint)?,
        None => join(&parse_url(authorization_server_url)?, "/authorize")?,
    };
    let pkce = yapi_ai::auth::pkce::generate();
    let mut params: Vec<(String, String)> = url.query_pairs().into_owned().collect();
    set(&mut params, "response_type", "code");
    set(
        &mut params,
        "client_id",
        text(client, "client_id").unwrap_or_default(),
    );
    set(&mut params, "code_challenge", &pkce.challenge);
    set(&mut params, "code_challenge_method", "S256");
    set(&mut params, "redirect_uri", redirect_url);
    if !state.is_empty() {
        set(&mut params, "state", state);
    }
    if let Some(scope) = scope {
        set(&mut params, "scope", scope);
        if scope
            .split_whitespace()
            .any(|word| word == "offline_access")
        {
            set(&mut params, "prompt", "consent");
        }
    }
    if let Some(resource) = resource {
        set(&mut params, "resource", resource);
    }
    url.query_pairs_mut().clear().extend_pairs(&params);
    Ok((url, pkce.verifier))
}

async fn run_flow(
    provider: &Provider<'_>,
    options: &FlowOptions,
) -> Result<FlowResult, OAuthError> {
    let fetch = options.fetch;
    let metadata_url = options
        .authorization_server_metadata_url
        .as_deref()
        .map(secure_endpoint)
        .transpose()?;
    // With a configured metadata URL, discovery is not cached, so changing it applies at once.
    let cached = match metadata_url {
        Some(_) => None,
        None => provider.stored("discovery").await?,
    };
    let discovered = match cached.as_ref().and_then(|cached| {
        truthy(cached, "authorizationServerUrl").map(|url| (cached, url.to_owned()))
    }) {
        Some((cached, url)) => {
            let metadata = match cached
                .get("authorizationServerMetadata")
                .and_then(Value::as_object)
            {
                Some(metadata) => Some(metadata.clone()),
                None => discover_authorization_server_metadata(&url, fetch).await?,
            };
            let resource = cached
                .get("resourceMetadata")
                .and_then(Value::as_object)
                .cloned();
            server_info(url, metadata, resource)
        }
        None => {
            discover_server_info(
                &options.server_url,
                options.resource_metadata_url.as_deref(),
                metadata_url.as_ref(),
                fetch,
            )
            .await?
        }
    };
    if metadata_url.is_none() {
        let mut discovery = discovered.clone();
        if let Some(url) = &options.resource_metadata_url {
            discovery.insert("resourceMetadataUrl".into(), Value::String(url.clone()));
        }
        provider
            .update(|state| {
                state.insert("discovery".into(), Value::Object(discovery));
            })
            .await?;
    }
    let authorization_server = text(&discovered, "authorizationServerUrl")
        .unwrap_or_default()
        .to_owned();
    let metadata = discovered
        .get("authorizationServerMetadata")
        .and_then(Value::as_object);
    let resource_metadata = discovered
        .get("resourceMetadata")
        .and_then(Value::as_object);
    let resource = select_resource(&options.server_url, resource_metadata)?;
    // An empty scope falls through to the next source.
    let scope = options
        .scope
        .clone()
        .filter(|scope| !scope.is_empty())
        .or_else(|| {
            resource_metadata
                .and_then(|resource| list(resource, "scopes_supported"))
                .map(|scopes| scopes.join(" "))
                .filter(|scope| !scope.is_empty())
        })
        .or_else(|| truthy(&provider.client_metadata, "scope").map(str::to_owned));
    let client = match provider.client_information().await? {
        Some(client) => client,
        None => {
            if options.authorization_code.is_some() {
                return Err(other(
                    "OAuth client information is missing during code exchange",
                ));
            }
            let client = register_client(
                &authorization_server,
                metadata,
                &provider.client_metadata,
                scope.as_deref(),
                fetch,
            )
            .await?;
            provider.save_client_information(client.clone()).await?;
            client
        }
    };
    let request = TokenRequest {
        authorization_server_url: &authorization_server,
        metadata,
        client: &client,
        resource: resource.as_deref(),
        fetch,
    };
    if let Some(code) = &options.authorization_code {
        // RFC 9207: never send a code from another authorization server to this one.
        if let Some(metadata) = metadata
            && (options.iss.is_some()
                || metadata.get("authorization_response_iss_parameter_supported")
                    == Some(&Value::Bool(true)))
        {
            let issuer = text(metadata, "issuer").unwrap_or_default();
            if options.iss.as_deref() != Some(issuer) {
                return Err(issuer_mismatch(issuer, options.iss.as_deref()));
            }
        }
        let verifier = provider.code_verifier().await?;
        let tokens = request
            .send(vec![
                ("grant_type".into(), "authorization_code".into()),
                ("code".into(), code.clone()),
                ("code_verifier".into(), verifier),
                ("redirect_uri".into(), provider.redirect_url.clone()),
            ])
            .await?;
        provider
            .save_tokens(with_scope(tokens, scope.as_deref()))
            .await?;
        return Ok(FlowResult::Authorized);
    }
    let existing = match options.skip_refresh {
        true => None,
        false => provider.stored("tokens").await?,
    };
    if let Some(existing) = &existing
        && let Some(refresh_token) = truthy(existing, "refresh_token")
    {
        let refreshed = request
            .send(vec![
                ("grant_type".into(), "refresh_token".into()),
                ("refresh_token".into(), refresh_token.to_owned()),
            ])
            .await;
        match refreshed {
            Ok(tokens) => {
                // `{refresh_token, ...tokens}`: a new refresh token replaces the old one in place.
                let mut merged = Object::new();
                merged.insert("refresh_token".into(), Value::from(refresh_token));
                merged.extend(tokens);
                // A refresh without `scope` keeps the scope of the grant.
                provider
                    .save_tokens(with_scope(merged, text(existing, "scope")))
                    .await?;
                return Ok(FlowResult::Authorized);
            }
            Err(error @ OAuthError::InsecureEndpoint(_)) => return Err(error),
            Err(OAuthError::Response { code, message }) if code != "server_error" => {
                return Err(OAuthError::Response { code, message });
            }
            Err(_) => {}
        }
    }
    let state = provider.state().await?;
    let (url, verifier) = start_authorization(
        &authorization_server,
        metadata,
        &client,
        &provider.redirect_url,
        scope.as_deref(),
        &state,
        resource.as_deref(),
    )?;
    provider
        .update(|state| {
            state.insert("codeVerifier".into(), Value::String(verifier));
        })
        .await?;
    Ok(FlowResult::Redirect(url.to_string()))
}

/// pi's `authorizeMcp`: exchanges a code, refreshes stored tokens, or starts
/// an authorization. A rejected client registers again and a rejected grant
/// is dropped, then the flow runs once more.
pub async fn authorize(
    provider: &Provider<'_>,
    options: &FlowOptions,
) -> Result<FlowResult, OAuthError> {
    match run_flow(provider, options).await {
        Err(OAuthError::Response { code, .. })
            if code == "invalid_client" || code == "unauthorized_client" =>
        {
            provider.invalidate(Invalidate::All).await?;
            run_flow(provider, options).await
        }
        Err(OAuthError::Response { code, .. }) if code == "invalid_grant" => {
            provider.invalidate(Invalidate::Tokens).await?;
            run_flow(provider, options).await
        }
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_challenges_like_pi() {
        let challenge = parse_www_authenticate(Some(
            r#"Bearer resource_metadata="https://x.test/.well-known/oauth-protected-resource/mcp", scope="", error=insufficient_scope"#,
        ));
        assert_eq!(
            challenge,
            Challenge {
                resource_metadata_url: Some(
                    "https://x.test/.well-known/oauth-protected-resource/mcp".into()
                ),
                scope: None,
                error: Some("insufficient_scope".into()),
                error_description: None,
            }
        );
        assert_eq!(
            parse_www_authenticate(Some("Basic realm=x")),
            Challenge::default()
        );
        assert!(is_insufficient_scope(Some(
            "Bearer error=\"insufficient_scope\""
        )));
        assert!(!is_insufficient_scope(Some("Bearer error=invalid_token")));
    }

    #[test]
    fn builds_discovery_urls_like_pi() {
        let urls = discovery_urls("https://auth.test/tenant/").unwrap();
        let urls: Vec<String> = urls.into_iter().map(String::from).collect();
        assert_eq!(
            urls,
            [
                "https://auth.test/.well-known/oauth-authorization-server/tenant",
                "https://auth.test/.well-known/openid-configuration/tenant",
                "https://auth.test/tenant/.well-known/openid-configuration",
            ]
        );
        assert_eq!(discovery_urls("https://auth.test").unwrap().len(), 2);
    }

    #[test]
    fn selects_the_protected_resource() {
        let metadata = |resource: &str| {
            json!({"resource": resource})
                .as_object()
                .cloned()
                .unwrap_or_default()
        };
        assert_eq!(
            select_resource("https://x.test/mcp#a", Some(&metadata("https://x.test/"))),
            Ok(Some("https://x.test/".into()))
        );
        assert_eq!(
            select_resource(
                "https://x.test/mcp",
                Some(&metadata("https://x.test/other"))
            ),
            Err(other(
                "Protected resource https://x.test/other does not match MCP server https://x.test/mcp"
            ))
        );
        assert_eq!(select_resource("https://x.test/mcp", None), Ok(None));
    }

    #[test]
    fn parses_tokens_and_clients_like_pi() {
        let tokens = parse_tokens(&json!({
            "token_type": "Bearer", "access_token": "a", "scope": "", "refresh_token": null, "expires_in": "60",
        }))
        .unwrap();
        assert_eq!(
            Value::Object(tokens),
            json!({"access_token": "a", "token_type": "Bearer", "expires_in": 60})
        );
        assert_eq!(
            parse_tokens(&json!({"access_token": "a", "token_type": "Bearer", "expires_in": "x"})),
            Err(other("Invalid expires_in"))
        );
        let client = parse_client_information(&json!({
            "client_name": "n", "client_secret": "", "client_id": "c", "extra": 1,
        }))
        .unwrap();
        assert_eq!(
            yapi_types::json::stringify(&client),
            r#"{"client_name":"n","client_id":"c","extra":1,"redirect_uris":[]}"#
        );
    }

    #[test]
    fn picks_client_authentication_like_pi() {
        let client = |value: Value| value.as_object().cloned().unwrap_or_default();
        let secret = client(json!({"client_id": "c", "client_secret": "s"}));
        assert!(select_client_auth(&secret, &[]) == ClientAuth::Basic);
        assert!(select_client_auth(&secret, &["client_secret_post"]) == ClientAuth::Post);
        let public = client(json!({"client_id": "c"}));
        assert!(select_client_auth(&public, &["client_secret_basic"]) == ClientAuth::None);
        let hinted = client(
            json!({"client_id": "c", "client_secret": "s", "token_endpoint_auth_method": "client_secret_post"}),
        );
        assert!(select_client_auth(&hinted, &[]) == ClientAuth::Post);
    }

    #[test]
    fn sends_credentials_only_over_https_or_loopback() {
        assert!(secure_endpoint("http://auth.test/token").is_err());
        assert!(secure_endpoint("http://127.0.0.1:1/token").is_ok());
    }
}
