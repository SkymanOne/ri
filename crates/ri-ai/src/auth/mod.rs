//! Provider authentication: the `auth.json` credential store, OAuth sign-in
//! and refresh, and the interaction a sign-in uses to talk to the user.
//!
//! Ports of `packages/ai/src/auth` in pi `v1.0.0`.

pub mod anthropic;
pub mod callback;
pub mod chatgpt;
pub mod codex;
pub mod copilot;
pub mod device;
pub mod federation;
pub mod google_adc;
mod lock;
pub mod pkce;
pub mod store;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use ri_types::auth::OAuthCredential;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

pub use store::{CredentialKind, CredentialStore};

/// A boxed future, for trait methods.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Why a sign-in, refresh or credential change failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    /// The user or the caller cancelled.
    #[error("Login cancelled")]
    Cancelled,
    /// Anything else, with pi's message.
    #[error("{0}")]
    Failed(String),
}

impl AuthError {
    pub(crate) fn failed(message: impl Into<String>) -> AuthError {
        AuthError::Failed(message.into())
    }
}

/// One choice of a select prompt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectOption {
    /// Returned when chosen.
    pub id: String,
    /// Shown.
    pub label: String,
}

/// A question a sign-in asks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthPrompt {
    /// Free text.
    Text {
        /// The question.
        message: String,
        /// Example input.
        placeholder: Option<String>,
    },
    /// Text that must not be echoed, such as a key.
    Secret {
        /// The question.
        message: String,
    },
    /// One of several options; the answer is the option id.
    Select {
        /// The question.
        message: String,
        /// The choices.
        options: Vec<SelectOption>,
    },
    /// A pasted authorization code or redirect URL. A sign-in may withdraw it
    /// when the browser callback arrives first.
    ManualCode {
        /// The question.
        message: String,
        /// Example input.
        placeholder: Option<String>,
    },
}

/// A labelled link in an info event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    /// Shown.
    pub label: String,
    /// Target.
    pub url: String,
}

/// Something a sign-in tells the user.
#[derive(Clone, Debug, PartialEq)]
pub enum AuthEvent {
    /// Information with optional links.
    Info {
        /// Text.
        message: String,
        /// Links.
        links: Vec<Link>,
    },
    /// A URL to open in the browser.
    AuthUrl {
        /// The authorization URL.
        url: String,
        /// What to do there.
        instructions: Option<String>,
    },
    /// A device code to enter at a verification page.
    DeviceCode {
        /// Code to enter.
        user_code: String,
        /// Page to enter it at.
        verification_uri: String,
        /// Polling interval.
        interval_seconds: Option<f64>,
        /// Lifetime of the code.
        expires_in_seconds: Option<f64>,
    },
    /// Progress text.
    Progress {
        /// Text.
        message: String,
    },
}

/// A request from a sign-in to the UI.
#[derive(Debug)]
pub enum AuthRequest {
    /// Ask a question. Dropping `reply` cancels the sign-in; `cancel` fires when
    /// the sign-in no longer needs the answer.
    Prompt {
        /// The question.
        prompt: AuthPrompt,
        /// Where the answer goes.
        reply: oneshot::Sender<String>,
        /// Fires when the prompt is withdrawn.
        cancel: CancellationToken,
    },
    /// Show something.
    Notify(AuthEvent),
}

/// The channel between a running sign-in and the UI that serves it.
#[derive(Clone, Debug)]
pub struct Interaction {
    requests: mpsc::UnboundedSender<AuthRequest>,
    cancel: CancellationToken,
}

impl Interaction {
    /// An interaction cancelled by `cancel`, with the receiver the UI reads.
    pub fn new(cancel: CancellationToken) -> (Interaction, mpsc::UnboundedReceiver<AuthRequest>) {
        let (requests, receiver) = mpsc::unbounded_channel();
        (Interaction { requests, cancel }, receiver)
    }

    /// Cancels the whole sign-in.
    pub fn cancel(&self) -> &CancellationToken {
        &self.cancel
    }

    /// Asks the UI and waits for the answer.
    pub async fn prompt(&self, prompt: AuthPrompt) -> Result<String, AuthError> {
        self.prompt_until(prompt, self.cancel.child_token()).await
    }

    /// Asks the UI; `cancel` withdraws the question.
    pub async fn prompt_until(
        &self,
        prompt: AuthPrompt,
        cancel: CancellationToken,
    ) -> Result<String, AuthError> {
        if self.cancel.is_cancelled() {
            return Err(AuthError::Cancelled);
        }
        let (reply, answer) = oneshot::channel();
        self.requests
            .send(AuthRequest::Prompt {
                prompt,
                reply,
                cancel: cancel.clone(),
            })
            .map_err(|_| AuthError::Cancelled)?;
        tokio::select! {
            answer = answer => answer.map_err(|_| AuthError::Cancelled),
            () = cancel.cancelled() => Err(AuthError::Cancelled),
        }
    }

    /// Tells the UI something.
    pub fn notify(&self, event: AuthEvent) {
        let _ = self.requests.send(AuthRequest::Notify(event));
    }

    /// Fails with [`AuthError::Cancelled`] once cancelled.
    pub fn check(&self) -> Result<(), AuthError> {
        if self.cancel.is_cancelled() {
            Err(AuthError::Cancelled)
        } else {
            Ok(())
        }
    }
}

/// Installation details some sign-ins need.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoginOptions {
    /// The installation's device id, a UUID kept in global settings.
    pub device_id: Option<String>,
}

/// Credentials derived from an OAuth token for one request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OAuthAuth {
    /// Sent as the API key.
    pub api_key: String,
    /// Replaces the model's base URL, for providers whose endpoint depends on
    /// the account.
    pub base_url: Option<String>,
}

/// An account sign-in: log in, refresh, and derive request credentials.
pub trait OAuthProvider: Send + Sync {
    /// Method name, such as `Anthropic (Claude Pro/Max)`.
    fn name(&self) -> &str;

    /// Label of the login menu entry, when the provider sets one.
    fn login_label(&self) -> Option<&str> {
        None
    }

    /// Whether the account is a subscription, which the login menu labels.
    fn is_subscription(&self) -> bool {
        true
    }

    /// Runs the sign-in.
    fn login<'a>(
        &'a self,
        interaction: &'a Interaction,
        options: &'a LoginOptions,
    ) -> BoxFuture<'a, Result<OAuthCredential, AuthError>>;

    /// Exchanges the refresh token for a new credential.
    fn refresh<'a>(
        &'a self,
        credential: &'a OAuthCredential,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<OAuthCredential, AuthError>>;

    /// Request credentials for a valid token.
    fn to_auth(&self, credential: &OAuthCredential) -> OAuthAuth {
        OAuthAuth {
            api_key: credential.access.clone(),
            base_url: None,
        }
    }
}

/// The built-in sign-in for a provider id, if ri implements it.
pub fn builtin_oauth(provider: &str) -> Option<Arc<dyn OAuthProvider>> {
    match provider {
        "anthropic" => Some(Arc::new(anthropic::AnthropicOAuth::default())),
        "openai-codex" => Some(Arc::new(codex::CodexOAuth::default())),
        "openai" => Some(Arc::new(chatgpt::ChatGptOAuth::default())),
        "github-copilot" => Some(Arc::new(copilot::CopilotOAuth::default())),
        _ => None,
    }
}

/// Unix time in milliseconds.
pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Milliseconds since the epoch of an RFC 3339 time such as
/// `2026-10-04T12:00:00Z` or `2026-10-04T12:00:00.5+02:00`.
pub(crate) fn rfc3339_ms(text: &str) -> Option<u64> {
    let (date, time) = text.trim().split_once(['T', 't', ' '])?;
    let (time, offset_seconds) = if let Some(time) = time.strip_suffix(['Z', 'z']) {
        (time, 0)
    } else {
        let at = time.rfind(['+', '-'])?;
        let (clock, offset) = time.split_at(at);
        let sign = if offset.starts_with('-') { -1 } else { 1 };
        let mut parts = offset[1..].split(':').map(|part| part.parse::<i64>().ok());
        let (hours, minutes) = (parts.next()??, parts.next().flatten().unwrap_or(0));
        (clock, sign * (hours * 3600 + minutes * 60))
    };
    let mut date = date.split('-').map(|part| part.parse::<i64>().ok());
    let (year, month, day) = (date.next()??, date.next()??, date.next()??);
    let time = time.split('.').next()?;
    let mut time = time.split(':').map(|part| part.parse::<i64>().ok());
    let (hour, minute, second) = (time.next()??, time.next()??, time.next()??);
    // Days from the civil date, after Howard Hinnant's algorithm.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let seconds = days * 86_400 + hour * 3600 + minute * 60 + second - offset_seconds;
    u64::try_from(seconds).ok().map(|s| s * 1000)
}

/// pi's `OAUTH_CALLBACK_HOST`: `PI_OAUTH_CALLBACK_HOST`, else `127.0.0.1`.
pub(crate) fn callback_host() -> String {
    std::env::var("PI_OAUTH_CALLBACK_HOST")
        .ok()
        .filter(|host| !host.trim().is_empty())
        .unwrap_or_else(|| "127.0.0.1".into())
}

/// Splits pasted sign-in input into code and state: a redirect URL, `code#state`,
/// a `code=` query, or a bare code. Port of `parseAuthorizationInput`.
pub(crate) fn parse_authorization_input(input: &str) -> (Option<String>, Option<String>) {
    let value = input.trim();
    if value.is_empty() {
        return (None, None);
    }
    if let Ok(url) = url::Url::parse(value) {
        let get = |name: &str| {
            url.query_pairs()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.into_owned())
        };
        return (get("code"), get("state"));
    }
    if let Some((code, state)) = value.split_once('#') {
        let state = state.split('#').next().unwrap_or_default();
        return (Some(code.to_owned()), Some(state.to_owned()));
    }
    if value.contains("code=") {
        let pairs: Vec<(String, String)> = url::form_urlencoded::parse(value.as_bytes())
            .into_owned()
            .collect();
        let get = |name: &str| {
            pairs
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        };
        return (get("code"), get("state"));
    }
    (Some(value.to_owned()), None)
}

/// An `application/x-www-form-urlencoded` body.
pub(crate) fn form(pairs: &[(&str, &str)]) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in pairs {
        serializer.append_pair(key, value);
    }
    serializer.finish()
}

/// A JSON object of string fields, in order.
pub(crate) fn object(pairs: &[(&str, &str)]) -> serde_json::Value {
    serde_json::Value::Object(
        pairs
            .iter()
            .map(|(key, value)| {
                (
                    (*key).to_owned(),
                    serde_json::Value::String((*value).to_owned()),
                )
            })
            .collect(),
    )
}

/// The body parsed as JSON; `null` when it is not JSON.
pub(crate) async fn json_body(response: reqwest::Response) -> serde_json::Value {
    let bytes = response.bytes().await.unwrap_or_default();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

/// The text of an HTTP error for messages: the body, else the status text.
pub(crate) async fn error_text(response: reqwest::Response) -> String {
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if text.is_empty() {
        status.canonical_reason().unwrap_or_default().to_owned()
    } else {
        text
    }
}

/// Sends `request`, failing with [`AuthError::Cancelled`] on cancellation.
pub(crate) async fn send(
    request: reqwest::RequestBuilder,
    cancel: &CancellationToken,
) -> Result<reqwest::Response, AuthError> {
    tokio::select! {
        response = request.send() => response.map_err(|err| AuthError::Failed(network_message(&err))),
        () = cancel.cancelled() => Err(AuthError::Cancelled),
    }
}

/// A network error with its causes, like Node's `fetch failed` chain.
pub(crate) fn network_message(err: &dyn std::error::Error) -> String {
    let mut message = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rfc3339_times() {
        assert_eq!(rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            rfc3339_ms("2026-10-04T12:30:15.5Z"),
            Some(1_791_117_015_000)
        );
        assert_eq!(
            rfc3339_ms("2026-10-04T14:30:15+02:00"),
            Some(1_791_117_015_000)
        );
        assert_eq!(rfc3339_ms("2026-10-04"), None);
    }

    #[test]
    fn parses_pasted_input_like_pi() {
        let parse = |input| parse_authorization_input(input);
        assert_eq!(
            parse("http://localhost:1455/auth/callback?code=c1&state=s1"),
            (Some("c1".into()), Some("s1".into()))
        );
        assert_eq!(parse("c2#s2"), (Some("c2".into()), Some("s2".into())));
        assert_eq!(
            parse("code=c3&state=s3"),
            (Some("c3".into()), Some("s3".into()))
        );
        assert_eq!(parse(" c4 "), (Some("c4".into()), None));
        assert_eq!(parse(""), (None, None));
    }

    #[tokio::test]
    async fn prompts_answer_and_cancel() {
        let cancel = CancellationToken::new();
        let (interaction, mut requests) = Interaction::new(cancel.clone());
        let asking = tokio::spawn({
            let interaction = interaction.clone();
            async move {
                interaction
                    .prompt(AuthPrompt::Secret {
                        message: "Enter key".into(),
                    })
                    .await
            }
        });
        let Some(AuthRequest::Prompt { reply, .. }) = requests.recv().await else {
            panic!("expected a prompt");
        };
        reply.send("secret".into()).unwrap();
        assert_eq!(asking.await.unwrap().as_deref(), Ok("secret"));
        let dropped = tokio::spawn({
            let interaction = interaction.clone();
            async move {
                interaction
                    .prompt(AuthPrompt::Text {
                        message: "?".into(),
                        placeholder: None,
                    })
                    .await
            }
        });
        drop(requests.recv().await);
        assert_eq!(dropped.await.unwrap(), Err(AuthError::Cancelled));
        cancel.cancel();
        assert_eq!(interaction.check(), Err(AuthError::Cancelled));
    }
}
