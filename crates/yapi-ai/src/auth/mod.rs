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
pub mod kimi;
pub mod lock;
pub mod meta;
pub mod openrouter;
pub mod pkce;
pub mod radius;
pub mod store;
pub mod xai;

use std::sync::Arc;

use futures_util::future::BoxFuture;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use yapi_types::auth::OAuthCredential;

pub use store::{CredentialKind, CredentialStore};

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

impl AuthPrompt {
    /// A [`AuthPrompt::Select`] of `(id, label)` options.
    pub(crate) fn select(message: impl Into<String>, options: &[(&str, &str)]) -> AuthPrompt {
        AuthPrompt::Select {
            message: message.into(),
            options: options
                .iter()
                .map(|(id, label)| SelectOption {
                    id: (*id).to_owned(),
                    label: (*label).to_owned(),
                })
                .collect(),
        }
    }
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
        match cancel.run_until_cancelled(answer).await {
            Some(Ok(answer)) => Ok(answer),
            _ => Err(AuthError::Cancelled),
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
    pub api_key: Option<String>,
    /// Headers that carry the token instead; `None` removes a header.
    pub headers: indexmap::IndexMap<String, Option<String>>,
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
    fn to_auth<'a>(
        &'a self,
        credential: &'a OAuthCredential,
    ) -> BoxFuture<'a, Result<OAuthAuth, AuthError>> {
        Box::pin(async move {
            Ok(OAuthAuth {
                api_key: Some(credential.access.clone()),
                ..OAuthAuth::default()
            })
        })
    }
}

/// The built-in sign-in for a provider id, if yapi implements it.
pub fn builtin_oauth(provider: &str) -> Option<Arc<dyn OAuthProvider>> {
    match provider {
        "anthropic" => Some(Arc::new(anthropic::AnthropicOAuth::default())),
        "openai-codex" => Some(Arc::new(codex::CodexOAuth::default())),
        "openai" => Some(Arc::new(chatgpt::ChatGptOAuth::default())),
        "github-copilot" => Some(Arc::new(copilot::CopilotOAuth::default())),
        "kimi-coding" => Some(Arc::new(kimi::KimiOAuth::default())),
        "meta" => Some(Arc::new(meta::MetaOAuth::default())),
        "xai" => Some(Arc::new(xai::XaiOAuth::default())),
        "openrouter" => Some(Arc::new(openrouter::OpenRouterOAuth::default())),
        "radius" => Some(Arc::new(radius::RadiusOAuth::new(
            "Radius",
            radius::DEFAULT_GATEWAY,
        ))),
        _ => None,
    }
}

pub(crate) use yapi_types::time::now_ms;

/// pi's `openBrowser`: opens `target` in the platform browser, best effort and
/// without a shell.
pub fn open_browser(target: &str) {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(program)
        .arg(target)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
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
        return (
            callback::query(&url, "code"),
            callback::query(&url, "state"),
        );
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
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish()
}

/// A POST of `fields` as a form that accepts JSON, without a timeout.
pub fn post_form(url: impl reqwest::IntoUrl, fields: &[(&str, &str)]) -> reqwest::RequestBuilder {
    crate::http::client()
        .post(url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(form(fields))
}

/// `Date.now() + expires_in * 1000 - margin`, in milliseconds.
pub(crate) fn expiry(expires_in: f64, margin_ms: f64) -> u64 {
    let value = now_ms() as f64 + expires_in * 1000.0 - margin_ms;
    if value.is_finite() && value > 0.0 {
        value as u64
    } else {
        0
    }
}

/// A JSON object of string fields, in order.
pub(crate) fn object(pairs: &[(&str, &str)]) -> serde_json::Value {
    pairs.iter().copied().collect()
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
    cancel
        .run_until_cancelled(request.send())
        .await
        .ok_or(AuthError::Cancelled)?
        .map_err(|err| AuthError::Failed(network_message(&err)))
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
