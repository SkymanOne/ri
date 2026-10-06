//! OAuth sign-in for remote MCP servers. Port of `extensions/mcp/oauth.ts` in
//! pi `v1.0.0`.
//!
//! Connections never start a browser flow on their own. They send the stored
//! access token and, after a 401, try the stored refresh token. When that is
//! not possible they fail with [`McpError::SignInRequired`], and the user
//! signs in with `/mcp login` or `yapi mcp login`, which run the
//! authorization code flow (PKCE, dynamic client registration) against a
//! loopback callback.
//!
//! Credentials live in `<agent-dir>/mcp-auth.json`, keyed by server name and
//! URL, in pi's format, so pi and yapi share sign-ins.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::BoxFuture;
use reqwest::Url;
use serde_json::Value;
use sha2::Digest as _;
use tokio_util::sync::CancellationToken;
use yapi_ai::auth::AuthError;
use yapi_ai::auth::callback::{Reply, Server, error_page, query, success_page};
use yapi_ai::auth::lock::FileLock;
use yapi_types::config::ConfigFile;
use yapi_types::sync::lock;

use super::config::{OAuthConfig, namespace};
use super::connection::resolve_value;
use super::jsonrpc::McpError;
use super::oauth::{
    Challenge, Fetch, FlowOptions, FlowResult, OAuthError, Object, Provider, authorize,
    parse_www_authenticate, text,
};
use crate::config::APP_NAME;

const CALLBACK_HOST: &str = "127.0.0.1";
/// Redirect URI for refreshes when none is stored. Refreshing never redirects
/// the user.
const FALLBACK_REDIRECT_URL: &str = "http://127.0.0.1/callback";
/// Access tokens this close to expiry are refreshed before they are sent.
const REFRESH_SKEW_MS: f64 = 30_000.0;
/// Bounds each request of a refresh.
const REFRESH_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// How long the callback server waits for the browser.
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(5 * 60);

fn mcp_error(error: OAuthError) -> McpError {
    match error {
        OAuthError::Network(message) => McpError::Network(message),
        error => McpError::Other(error.to_string()),
    }
}

fn failed(error: impl std::fmt::Display) -> AuthError {
    AuthError::Failed(error.to_string())
}

// ---------------------------------------------------------------------------
// Credential store
// ---------------------------------------------------------------------------

/// pi's `McpOAuthCredentialStore`: the OAuth state of every server (client
/// registration, tokens, pending PKCE verifier) in `mcp-auth.json`.
#[derive(Clone, Debug)]
pub struct CredentialStore {
    path: PathBuf,
}

/// The state's keys: by name and URL, so servers sharing a URL keep separate
/// accounts, and the legacy key by URL alone.
fn store_keys(name: &str, server_url: &str) -> (String, String) {
    let legacy = Url::parse(server_url).map_or_else(|_| server_url.to_owned(), String::from);
    (format!("{}|{legacy}", namespace(name)), legacy)
}

fn parse_states(content: &str) -> Result<Object, AuthError> {
    if content.trim().is_empty() {
        return Ok(Object::new());
    }
    match serde_json::from_str::<Value>(content) {
        Ok(Value::Object(states)) => Ok(states),
        Ok(_) => Ok(Object::new()),
        Err(error) => Err(failed(error)),
    }
}

impl CredentialStore {
    /// The store in `agent_dir`.
    pub fn new(agent_dir: &Path) -> CredentialStore {
        CredentialStore {
            path: agent_dir.join(ConfigFile::McpAuth.file_name()),
        }
    }

    /// The state of server `name` at `server_url`.
    pub fn for_server(&self, name: &str, server_url: &str) -> ServerStore {
        let (key, legacy_key) = store_keys(name, server_url);
        ServerStore {
            store: self.clone(),
            key,
            legacy_key,
        }
    }

    /// Runs `change` on the stored states under the file lock; writes them
    /// back when it returns true.
    async fn with_states<T>(
        &self,
        change: impl FnOnce(&mut Object) -> (T, bool),
    ) -> Result<T, McpError> {
        yapi_ai::auth::store::with_file_lock(&self.path, |current| {
            let mut states = parse_states(current)?;
            let (result, write) = change(&mut states);
            let next = match write {
                true => Some(ConfigFile::McpAuth.render(&states).map_err(failed)?),
                false => None,
            };
            Ok((result, next))
        })
        .await
        .map_err(|error| McpError::Other(error.to_string()))
    }

    /// The stored tokens of a server, for noticing sign-ins of another
    /// process. Does not take over legacy state.
    pub async fn tokens(&self, name: &str, server_url: &str) -> Result<Option<Value>, McpError> {
        let (key, legacy) = store_keys(name, server_url);
        self.with_states(|states| {
            let state = states.get(&key).or_else(|| states.get(&legacy));
            (state.and_then(|state| state.get("tokens")).cloned(), false)
        })
        .await
    }

    /// Removes the server's credentials; whether any were stored.
    pub async fn remove(&self, name: &str, server_url: &str) -> Result<bool, McpError> {
        let (key, legacy) = store_keys(name, server_url);
        self.with_states(|states| {
            let removed =
                states.shift_remove(&key).is_some() || states.shift_remove(&legacy).is_some();
            (removed, removed)
        })
        .await
    }
}

/// One server's state in the [`CredentialStore`].
#[derive(Clone, Debug)]
pub struct ServerStore {
    store: CredentialStore,
    key: String,
    legacy_key: String,
}

impl ServerStore {
    /// The stored state. The first server to load legacy state takes it over.
    pub async fn load(&self) -> Result<Option<Object>, McpError> {
        self.store
            .with_states(|states| {
                if states.contains_key(&self.key) || !states.contains_key(&self.legacy_key) {
                    return (
                        states.get(&self.key).and_then(Value::as_object).cloned(),
                        false,
                    );
                }
                let state = states.shift_remove(&self.legacy_key).unwrap_or_default();
                states.insert(self.key.clone(), state.clone());
                (state.as_object().cloned(), true)
            })
            .await
    }

    /// Replaces the stored state.
    pub async fn save(&self, state: Object) -> Result<(), McpError> {
        self.store
            .with_states(|states| {
                states.insert(self.key.clone(), Value::Object(state));
                ((), true)
            })
            .await
    }

    /// Runs `run` while no other process refreshes the server's tokens, with
    /// pi's lock file for the server next to `mcp-auth.json`, whose directory
    /// exists once the tokens were loaded.
    async fn with_refresh_lock<T>(
        &self,
        run: impl Future<Output = Result<T, McpError>>,
    ) -> Result<T, McpError> {
        let digest = sha2::Sha256::digest(self.key.as_bytes());
        let hash = yapi_types::time::hex(&digest[..8]);
        let _lock = FileLock::acquire(
            &self
                .store
                .path
                .with_file_name(format!("mcp-auth-refresh-{hash}")),
            &CancellationToken::new(),
        )
        .await
        .map_err(|error| McpError::Other(error.to_string()))?;
        run.await
    }
}

// ---------------------------------------------------------------------------
// Auth provider
// ---------------------------------------------------------------------------

/// Where the loopback callback server listens and the redirect URI it serves.
struct CallbackSettings {
    /// Address to listen on.
    host: String,
    /// Host name in the redirect URI.
    redirect_host: String,
    port: Option<u16>,
    path: String,
    /// The exact redirect URI, when the port is fixed.
    fixed_redirect_url: Option<String>,
}

fn callback_settings(settings: &OAuthConfig) -> CallbackSettings {
    let configured = settings.callback_url.as_deref();
    let url = Url::parse(configured.unwrap_or(FALLBACK_REDIRECT_URL)).ok();
    let address = url
        .as_ref()
        .and_then(Url::host_str)
        .unwrap_or(CALLBACK_HOST)
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_owned();
    let url_port = url.as_ref().and_then(Url::port);
    let port = url_port.or(settings.callback_port);
    let path = url
        .as_ref()
        .map_or_else(|| "/callback".to_owned(), |url| url.path().to_owned());
    // A configured URI with a port is sent exactly as written, since servers compare it as a string.
    let fixed_redirect_url = match (url_port, port, url) {
        (Some(_), _, _) => configured.map(str::to_owned),
        (None, Some(port), Some(mut url)) => {
            let _ = url.set_port(Some(port));
            Some(url.to_string())
        }
        _ => None,
    };
    CallbackSettings {
        // `localhost` is served on 127.0.0.1; browsers fall back to it when ::1 refuses.
        host: if address == "localhost" {
            CALLBACK_HOST.to_owned()
        } else {
            address.clone()
        },
        redirect_host: address,
        port,
        path,
        fixed_redirect_url,
    }
}

/// pi's `mergeScopes`: the scopes of every list, each once.
fn merge_scopes(lists: &[Option<&str>]) -> Option<String> {
    let mut scopes: Vec<&str> = Vec::new();
    for word in lists
        .iter()
        .flatten()
        .flat_map(|list| list.split_whitespace())
    {
        if !scopes.contains(&word) {
            scopes.push(word);
        }
    }
    (!scopes.is_empty()).then(|| scopes.join(" "))
}

fn registered_redirect_urls(state: Option<&Object>) -> Vec<String> {
    state
        .and_then(|state| state.get("clientInformation"))
        .and_then(|client| client.get("redirect_uris"))
        .and_then(Value::as_array)
        .map(|urls| {
            urls.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn tokens(state: Option<&Object>) -> Option<&Object> {
    state?.get("tokens")?.as_object()
}

fn access_token(state: Option<&Object>) -> Option<String> {
    tokens(state)
        .and_then(|tokens| text(tokens, "access_token"))
        .map(str::to_owned)
}

fn has_refresh_token(state: Option<&Object>) -> bool {
    tokens(state)
        .and_then(|tokens| text(tokens, "refresh_token"))
        .is_some_and(|token| !token.is_empty())
}

/// What a sign-in asks of the user; pi's `McpSignInPrompt`.
pub struct SignInPrompt {
    /// Shows the authorization URL and opens it in a browser.
    pub show_authorization_url: Box<dyn Fn(&str) + Send + Sync>,
    /// Asks for the redirect URL from the browser's address bar, for when the
    /// browser cannot reach the loopback callback. The token withdraws the
    /// question once the callback arrives. `None` or empty text cancels the
    /// sign-in.
    pub redirect_url:
        Box<dyn Fn(CancellationToken) -> BoxFuture<'static, Option<String>> + Send + Sync>,
}

/// The OAuth credentials of one server: pi's `createMcpAuthProvider`. Sends
/// the stored access token and refreshes it when it is about to expire or
/// after a 401. Fails with [`McpError::SignInRequired`] when the user has to
/// sign in, including when the server asks for more scope.
///
/// Requests in this process refresh one at a time, and other processes are
/// kept out by pi's refresh lock, held from reading the tokens to saving new
/// ones. Tokens that changed meanwhile are used without refreshing, since
/// servers that rotate refresh tokens reject a second refresh with the old one.
pub struct McpAuth {
    name: String,
    server_url: String,
    store: ServerStore,
    config: OAuthConfig,
    /// The server's last challenge; sign-in uses its resource metadata URL
    /// and scope.
    challenge: Mutex<Option<Challenge>>,
    refreshing: tokio::sync::Mutex<()>,
}

impl McpAuth {
    /// The credentials of server `name` at `server_url`, with its `oauth`
    /// settings.
    pub fn new(
        name: &str,
        server_url: &str,
        credentials: &CredentialStore,
        config: OAuthConfig,
    ) -> McpAuth {
        McpAuth {
            name: name.to_owned(),
            server_url: server_url.to_owned(),
            store: credentials.for_server(name, server_url),
            config,
            challenge: Mutex::new(None),
            refreshing: tokio::sync::Mutex::new(()),
        }
    }

    /// The `oauth` settings with the client secret resolved.
    pub async fn settings(&self) -> Result<OAuthConfig, McpError> {
        let mut settings = self.config.clone();
        if let Some(secret) = &self.config.client_secret {
            let description = format!("MCP server \"{}\" oauth.clientSecret", self.name);
            settings.client_secret = Some(resolve_value(secret, &description).await?);
        }
        Ok(settings)
    }

    /// The last challenge the server sent.
    pub fn challenge(&self) -> Option<Challenge> {
        lock(&self.challenge).clone()
    }

    /// The access token to send, refreshed first when it is about to expire.
    /// A failed refresh sends the old token and lets a 401 decide.
    pub async fn token(&self) -> Result<Option<String>, McpError> {
        drop(self.refreshing.lock().await);
        let state = self.store.load().await?;
        let token = access_token(state.as_ref());
        let expired = state
            .as_ref()
            .and_then(|state| state.get("tokensExpireAt"))
            .and_then(Value::as_f64)
            .is_some_and(|at| at - REFRESH_SKEW_MS <= yapi_types::time::now_ms() as f64);
        if !expired || !has_refresh_token(state.as_ref()) {
            return Ok(token);
        }
        let _ = self.refresh(token.as_deref(), None).await;
        Ok(access_token(self.store.load().await?.as_ref()))
    }

    /// Handles a 401, or a 403 asking for more scope, to a request that sent
    /// `token`: refreshes, or fails when the user has to sign in.
    pub async fn on_unauthorized(
        &self,
        www_authenticate: Option<&str>,
        token: Option<&str>,
    ) -> Result<(), McpError> {
        let challenge = parse_www_authenticate(www_authenticate);
        *lock(&self.challenge) = Some(challenge.clone());
        // A refresh keeps the granted scope, so more scope needs a new sign-in.
        if challenge.error.as_deref() == Some("insufficient_scope") {
            return Err(McpError::SignInRequired);
        }
        self.refresh(token, Some(&challenge)).await
    }

    /// Waits for a running refresh, so shutdown does not drop rotated tokens
    /// before they are saved.
    pub async fn settled(&self) {
        drop(self.refreshing.lock().await);
    }

    /// Replaces `stale`, the access token that expired or was rejected.
    async fn refresh(
        &self,
        stale: Option<&str>,
        challenge: Option<&Challenge>,
    ) -> Result<(), McpError> {
        let _running = self.refreshing.lock().await;
        self.store
            .with_refresh_lock(async {
                let state = self.store.load().await?;
                if access_token(state.as_ref()).as_deref() != stale {
                    return Ok(());
                }
                if !has_refresh_token(state.as_ref()) {
                    return Err(McpError::SignInRequired);
                }
                let settings = self.settings().await?;
                let redirect_url = callback_settings(&settings)
                    .fixed_redirect_url
                    .or_else(|| registered_redirect_urls(state.as_ref()).into_iter().next())
                    .unwrap_or_else(|| FALLBACK_REDIRECT_URL.to_owned());
                let provider = self.provider(&settings, &redirect_url).map_err(mcp_error)?;
                let options = FlowOptions {
                    server_url: self.server_url.clone(),
                    resource_metadata_url: challenge
                        .and_then(|challenge| challenge.resource_metadata_url.clone()),
                    authorization_server_metadata_url: settings.auth_server_metadata_url.clone(),
                    scope: challenge.and_then(|challenge| challenge.scope.clone()),
                    fetch: Fetch {
                        timeout: Some(REFRESH_REQUEST_TIMEOUT),
                    },
                    ..FlowOptions::default()
                };
                match authorize(&provider, &options).await.map_err(mcp_error)? {
                    FlowResult::Authorized => Ok(()),
                    FlowResult::Redirect(_) => Err(McpError::SignInRequired),
                }
            })
            .await
    }

    fn provider<'a>(
        &'a self,
        settings: &OAuthConfig,
        redirect_url: &str,
    ) -> Result<Provider<'a>, OAuthError> {
        Provider::new(
            &self.server_url,
            redirect_url,
            settings.client_name.as_deref().unwrap_or(APP_NAME),
            settings.client_id.as_deref(),
            settings.client_secret.as_deref(),
            &self.store,
        )
    }

    /// pi's `signInMcpServer`: uses the stored refresh token when possible,
    /// otherwise runs the browser authorization code flow. Tokens are saved
    /// to the credential store, and the challenge that asked for the sign-in
    /// is answered.
    pub async fn sign_in(&self, prompt: &SignInPrompt) -> Result<(), AuthError> {
        let settings = self.settings().await.map_err(failed)?;
        let challenge = self.challenge();
        let stored = self.store.load().await.map_err(failed)?;
        let step_up = challenge
            .as_ref()
            .is_some_and(|challenge| challenge.error.as_deref() == Some("insufficient_scope"));
        let callback = callback_settings(&settings);
        // Reuse the port of the registered redirect URI so the registered client stays valid.
        let registered = registered_redirect_urls(stored.as_ref());
        let preferred_port = callback.port.or_else(|| {
            registered
                .first()
                .and_then(|url| Url::parse(url).ok())
                .and_then(|url| url.port())
        });
        let expected = Arc::new(Mutex::new(None));
        let mut server = listen(&callback, preferred_port, Arc::clone(&expected)).await?;
        let redirect_host = if callback.redirect_host.contains(':') {
            format!("[{}]", callback.redirect_host)
        } else {
            callback.redirect_host.clone()
        };
        let redirect_url = callback.fixed_redirect_url.clone().unwrap_or_else(|| {
            format!("http://{redirect_host}:{}{}", server.port(), callback.path)
        });
        if let Some(stored) = &stored {
            let mut next = stored.clone();
            // Every sign-in gets a fresh `state` parameter.
            next.shift_remove("oauthState");
            // A registered client cannot use another redirect URI, and its tokens belong to it.
            if settings.client_id.as_deref().is_none_or(str::is_empty)
                && !registered.contains(&redirect_url)
            {
                for key in ["clientInformation", "tokens", "tokensExpireAt"] {
                    next.shift_remove(key);
                }
            }
            self.store.save(next).await.map_err(failed)?;
        }
        let provider = self.provider(&settings, &redirect_url).map_err(failed)?;
        // A server asking for more scope gets it on top of the configured scope and, since the
        // challenge may list only the missing scopes, on top of the scope granted so far.
        let challenged = challenge
            .as_ref()
            .and_then(|challenge| challenge.scope.as_deref());
        let granted = (step_up && challenged.is_some())
            .then(|| tokens(stored.as_ref()).and_then(|tokens| text(tokens, "scope")))
            .flatten();
        let flow = FlowOptions {
            server_url: self.server_url.clone(),
            resource_metadata_url: challenge
                .as_ref()
                .and_then(|challenge| challenge.resource_metadata_url.clone()),
            authorization_server_metadata_url: settings.auth_server_metadata_url.clone(),
            scope: merge_scopes(&[settings.scope.as_deref(), granted, challenged]),
            // A refresh keeps the granted scope; more scope needs the browser flow.
            skip_refresh: step_up,
            ..FlowOptions::default()
        };
        if let FlowResult::Redirect(url) = authorize(&provider, &flow).await.map_err(failed)? {
            let state = provider.state().await.map_err(failed)?;
            (prompt.show_authorization_url)(&url);
            let (code, iss) = wait_for_response(&mut server, &expected, &state, prompt).await?;
            let exchange = FlowOptions {
                authorization_code: Some(code),
                iss,
                skip_refresh: false,
                ..flow
            };
            authorize(&provider, &exchange).await.map_err(failed)?;
        }
        *lock(&self.challenge) = None;
        Ok(())
    }
}

type Callback = (String, Option<String>);

/// Listens on `port`, or on a free port when it is taken and not fixed by the
/// settings. The server answers like pi-mcp's callback server.
async fn listen(
    settings: &CallbackSettings,
    port: Option<u16>,
    expected: Arc<Mutex<Option<String>>>,
) -> Result<Server<Callback>, AuthError> {
    let cancel = CancellationToken::new();
    let start = |port: u16| {
        let expected = Arc::clone(&expected);
        let path = settings.path.clone();
        let handler = move |_method: &str, url: &Url| -> Reply<Callback> {
            if url.path() != path {
                return Reply::page(404, error_page("Not found", None));
            }
            let state = query(url, "state");
            let mut pending = lock(&expected);
            if state.is_none() || *pending != state {
                return Reply::page(400, error_page("Invalid or expired OAuth state", None));
            }
            *pending = None;
            if let Some(error) = query(url, "error") {
                let description = query(url, "error_description").unwrap_or(error);
                return Reply {
                    status: 200,
                    html: error_page(
                        "Authorization failed. You may close this window.",
                        Some(&description),
                    ),
                    outcome: Some(Err(AuthError::Failed(description))),
                };
            }
            let Some(code) = query(url, "code") else {
                return Reply {
                    status: 400,
                    html: error_page("Missing authorization code", None),
                    outcome: Some(Err(failed(
                        "OAuth callback did not include an authorization code",
                    ))),
                };
            };
            let iss = query(url, "iss").filter(|iss| !iss.is_empty());
            Reply {
                status: 200,
                html: success_page("Signed in to the MCP server. You may now close this page."),
                outcome: Some(Ok((code, iss))),
            }
        };
        Server::start(
            &settings.host,
            port,
            &settings.path,
            &cancel,
            Box::new(handler),
        )
    };
    match start(port.unwrap_or(0)).await {
        Ok(server) => Ok(server),
        Err(error) if settings.port.is_some() || port.is_none() => Err(failed(error)),
        Err(_) => start(0).await.map_err(failed),
    }
}

fn response_from_redirect_url(input: &str, state: &str) -> Result<Callback, AuthError> {
    let url = Url::parse(input.trim())
        .map_err(|_| failed("Expected the full redirect URL from the browser address bar"))?;
    if let Some(error) = query(&url, "error") {
        return Err(AuthError::Failed(
            query(&url, "error_description").unwrap_or(error),
        ));
    }
    if query(&url, "state").as_deref() != Some(state) {
        return Err(failed("The redirect URL belongs to a different sign-in"));
    }
    let code = query(&url, "code")
        .ok_or_else(|| failed("The redirect URL does not contain an authorization code"))?;
    Ok((code, query(&url, "iss")))
}

/// The browser callback or a pasted redirect URL, whichever comes first.
async fn wait_for_response(
    server: &mut Server<Callback>,
    expected: &Mutex<Option<String>>,
    state: &str,
    prompt: &SignInPrompt,
) -> Result<Callback, AuthError> {
    *lock(expected) = Some(state.to_owned());
    let withdraw = CancellationToken::new();
    let _withdraw_on_return = withdraw.clone().drop_guard();
    let from_user = (prompt.redirect_url)(withdraw);
    tokio::select! {
        received = tokio::time::timeout(CALLBACK_TIMEOUT, server.wait()) => {
            received.unwrap_or_else(|_| Err(failed("OAuth callback timed out")))
        }
        input = from_user => match input {
            Some(input) if !input.trim().is_empty() => response_from_redirect_url(&input, state),
            _ => Err(AuthError::Cancelled),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn derives_callback_settings_like_pi() {
        let settings = |url: Option<&str>, port: Option<u16>| {
            callback_settings(&OAuthConfig {
                callback_url: url.map(str::to_owned),
                callback_port: port,
                ..OAuthConfig::default()
            })
        };
        let default = settings(None, None);
        assert_eq!(default.host, "127.0.0.1");
        assert_eq!(default.path, "/callback");
        assert_eq!(default.fixed_redirect_url, None);
        let fixed = settings(None, Some(8080));
        assert_eq!(
            fixed.fixed_redirect_url.as_deref(),
            Some("http://127.0.0.1:8080/callback")
        );
        let localhost = settings(Some("http://localhost:9000/oauth/cb"), None);
        assert_eq!(localhost.host, "127.0.0.1");
        assert_eq!(localhost.redirect_host, "localhost");
        assert_eq!(localhost.port, Some(9000));
        assert_eq!(
            localhost.fixed_redirect_url.as_deref(),
            Some("http://localhost:9000/oauth/cb")
        );
        assert_eq!(settings(Some("http://[::1]/cb"), None).host, "::1");
    }

    #[test]
    fn merges_scopes() {
        assert_eq!(
            merge_scopes(&[Some("a b"), None, Some("b c a")]).as_deref(),
            Some("a b c")
        );
        assert_eq!(merge_scopes(&[None, Some("")]), None);
    }

    #[test]
    fn reads_pasted_redirect_urls_like_pi() {
        assert_eq!(
            response_from_redirect_url(" http://127.0.0.1:1/callback?code=c&state=s&iss=i ", "s"),
            Ok(("c".into(), Some("i".into())))
        );
        let error = |input: &str| {
            response_from_redirect_url(input, "s")
                .err()
                .map(|error| error.to_string())
        };
        assert_eq!(
            error("code=c").as_deref(),
            Some("Expected the full redirect URL from the browser address bar")
        );
        assert_eq!(
            error("http://x/?error=denied&error_description=No").as_deref(),
            Some("No")
        );
        assert_eq!(
            error("http://x/?code=c&state=t").as_deref(),
            Some("The redirect URL belongs to a different sign-in")
        );
        assert_eq!(
            error("http://x/?state=s").as_deref(),
            Some("The redirect URL does not contain an authorization code")
        );
    }

    fn temp_store(name: &str) -> (PathBuf, CredentialStore) {
        let dir = std::env::temp_dir().join(format!("yapi-mcp-auth-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = CredentialStore::new(&dir);
        (dir, store)
    }

    const URL: &str = "https://mcp.example.com/mcp";

    fn state(token: &str) -> Object {
        json!({"serverUrl": URL, "tokens": {"access_token": token, "token_type": "Bearer"}})
            .as_object()
            .cloned()
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn keeps_separate_credentials_for_servers_sharing_a_url() {
        let (dir, store) = temp_store("shared");
        store
            .for_server("work", URL)
            .save(state("work-token"))
            .await
            .unwrap();
        store
            .for_server("personal", URL)
            .save(state("personal-token"))
            .await
            .unwrap();
        let token = |name: &'static str| {
            let store = store.clone();
            async move { access_token(store.for_server(name, URL).load().await.unwrap().as_ref()) }
        };
        assert_eq!(token("work").await.as_deref(), Some("work-token"));
        assert_eq!(token("personal").await.as_deref(), Some("personal-token"));
        assert!(store.remove("work", URL).await.unwrap());
        assert_eq!(store.for_server("work", URL).load().await.unwrap(), None);
        assert_eq!(
            store.tokens("personal", URL).await.unwrap(),
            Some(json!({"access_token": "personal-token", "token_type": "Bearer"}))
        );
        let text = std::fs::read_to_string(dir.join("mcp-auth.json")).unwrap();
        assert!(text.starts_with(
            "{\n  \"mcp__personal|https://mcp.example.com/mcp\": {\n    \"serverUrl\""
        ));
        assert!(text.ends_with("}\n"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn moves_legacy_credentials_to_the_first_server_that_loads_them() {
        let (dir, store) = temp_store("legacy");
        std::fs::create_dir_all(&dir).unwrap();
        let legacy = json!({URL: state("legacy-token")});
        std::fs::write(dir.join("mcp-auth.json"), legacy.to_string()).unwrap();
        // Reading tokens does not take the legacy state over.
        assert!(store.tokens("work", URL).await.unwrap().is_some());
        let loaded = store.for_server("my_work", URL).load().await.unwrap();
        assert_eq!(
            access_token(loaded.as_ref()).as_deref(),
            Some("legacy-token")
        );
        // Names differing only in `-` and `_` are the same server.
        assert!(store.tokens("my-work", URL).await.unwrap().is_some());
        assert_eq!(
            store.for_server("personal", URL).load().await.unwrap(),
            None
        );
        let text = std::fs::read_to_string(dir.join("mcp-auth.json")).unwrap();
        let keys: Vec<String> = serde_json::from_str::<Object>(&text)
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert_eq!(keys, [format!("mcp__my_work|{URL}")]);
        assert!(store.remove("my-work", URL).await.unwrap());
        assert!(!store.remove("my-work", URL).await.unwrap());
        let _ = std::fs::remove_dir_all(dir);
    }
}
