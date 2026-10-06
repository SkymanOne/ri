//! The loopback server that receives an OAuth redirect, and the race between
//! it and a pasted code. Port of `oauth/callback-server.ts` and
//! `utils/oauth-page.ts`.

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use url::Url;

use super::{AuthError, AuthPrompt, Interaction};

const PAGE: &str = include_str!("oauth-page.html");

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn render_page(title: &str, message: &str, details: Option<&str>) -> String {
    let details = details.map_or_else(String::new, |details| {
        format!("<div class=\"details\">{}</div>", escape_html(details))
    });
    PAGE.replace("__TITLE__", &escape_html(title))
        .replace("__HEADING__", &escape_html(title))
        .replace("__MESSAGE__", &escape_html(message))
        .replace("__DETAILS__", &details)
}

/// pi's success page.
pub fn success_page(message: &str) -> String {
    render_page("Authentication successful", message, None)
}

/// pi's failure page.
pub fn error_page(message: &str, details: Option<&str>) -> String {
    render_page("Authentication failed", message, details)
}

/// The reply to one request, and what it means for the sign-in.
pub(crate) struct Reply<T> {
    pub status: u16,
    pub html: String,
    /// Ends the wait with a value or an error.
    pub outcome: Option<Result<T, AuthError>>,
}

impl<T> Reply<T> {
    pub(crate) fn page(status: u16, html: String) -> Reply<T> {
        Reply {
            status,
            html,
            outcome: None,
        }
    }
}

type Handler<T> = Box<dyn FnMut(&str, &Url) -> Reply<T> + Send>;

/// A running loopback server. Dropping it closes the listener and every open
/// connection.
pub(crate) struct Server<T> {
    redirect_uri: String,
    outcome: oneshot::Receiver<Result<T, AuthError>>,
    cancel: CancellationToken,
    tasks: tokio::task::JoinHandle<()>,
}

impl<T: Send + 'static> Server<T> {
    /// Listens on `host:port` (0 picks a port) and answers each request with
    /// `handler`, until a reply carries an outcome.
    pub(crate) async fn start(
        host: &str,
        port: u16,
        path: &str,
        cancel: &CancellationToken,
        handler: Handler<T>,
    ) -> std::io::Result<Server<T>> {
        let listener = TcpListener::bind((host, port)).await?;
        let redirect_uri = format!("http://{host}:{}{path}", listener.local_addr()?.port());
        let (sender, outcome) = oneshot::channel();
        let state = Arc::new(Mutex::new((handler, Some(sender))));
        let tasks = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            while let Ok((stream, _)) = listener.accept().await {
                connections.spawn(serve_connection(stream, Arc::clone(&state)));
            }
        });
        Ok(Server {
            redirect_uri,
            outcome,
            cancel: cancel.clone(),
            tasks,
        })
    }

    /// `http://<host>:<port><path>`, with the port the listener bound.
    pub(crate) fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// The outcome of the first conclusive request.
    pub(crate) async fn wait(&mut self) -> Result<T, AuthError> {
        tokio::select! {
            outcome = &mut self.outcome => outcome.unwrap_or_else(|_| Err(AuthError::failed("OAuth callback server closed"))),
            () = self.cancel.cancelled() => Err(AuthError::Cancelled),
        }
    }
}

impl<T> Drop for Server<T> {
    fn drop(&mut self) {
        self.tasks.abort();
    }
}

type Shared<T> = Arc<Mutex<(Handler<T>, Option<oneshot::Sender<Result<T, AuthError>>>)>>;

async fn serve_connection<T>(mut stream: TcpStream, state: Shared<T>) {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 2048];
    while !buffer.windows(4).any(|window| window == b"\r\n\r\n") {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
        }
        if buffer.len() > 16 * 1024 {
            return;
        }
    }
    let head = String::from_utf8_lossy(&buffer);
    let mut parts = head.lines().next().unwrap_or_default().split(' ');
    let method = parts.next().unwrap_or_default().to_owned();
    let target = parts.next().unwrap_or("/");
    let url = Url::parse("http://localhost").and_then(|base| base.join(target));
    let (status, html, outcome) = match url {
        Ok(url) => {
            let mut guard = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let (handler, sender) = &mut *guard;
            let reply = handler(&method, &url);
            let outcome = reply
                .outcome
                .and_then(|outcome| sender.take().map(|sender| (sender, outcome)));
            (reply.status, reply.html, outcome)
        }
        Err(_) => (404, error_page("Callback route not found.", None), None),
    };
    let reason = reqwest::StatusCode::from_u16(status)
        .ok()
        .and_then(|code| code.canonical_reason())
        .unwrap_or("Error");
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: text/html; charset=utf-8\r\ncache-control: no-store\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{html}",
        html.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
    if let Some((sender, outcome)) = outcome {
        let _ = sender.send(outcome);
    }
}

/// The first value of the query parameter `name`.
pub(crate) fn query(url: &Url, name: &str) -> Option<String> {
    url.query_pairs()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
}

/// A server for pi's shared callback: GET `path` with `code` (and `state` when
/// expected) yields the code.
pub(crate) async fn start_code_server(
    provider_name: &str,
    host: &str,
    port: u16,
    path: &str,
    state: Option<String>,
    cancel: &CancellationToken,
) -> std::io::Result<Server<String>> {
    let provider = provider_name.to_owned();
    let expected_path = path.to_owned();
    let mut claimed = false;
    let handler = move |method: &str, url: &Url| -> Reply<String> {
        if method != "GET" || url.path() != expected_path {
            return Reply::page(404, error_page("Callback route not found.", None));
        }
        if let Some(state) = &state
            && query(url, "state").as_deref() != Some(state)
        {
            return Reply::page(400, error_page("State mismatch.", None));
        }
        if claimed {
            return Reply::page(
                409,
                error_page("This sign-in has already been handled.", None),
            );
        }
        if let Some(error) = query(url, "error") {
            let description = query(url, "error_description").unwrap_or(error);
            claimed = true;
            return Reply {
                status: 400,
                html: error_page(
                    &format!("{provider} authorization failed."),
                    Some(&description),
                ),
                outcome: Some(Err(AuthError::Failed(format!(
                    "{provider} authorization failed: {description}"
                )))),
            };
        }
        let Some(code) = query(url, "code") else {
            return Reply::page(400, error_page("Missing authorization code.", None));
        };
        claimed = true;
        Reply {
            status: 200,
            html: success_page(&format!(
                "Signed in to {provider}. You may now close this page."
            )),
            outcome: Some(Ok(code)),
        }
    };
    Server::start(host, port, path, cancel, Box::new(handler)).await
}

/// Where the authorization arrived from.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Received<T> {
    /// The browser reached the loopback server.
    Callback(T),
    /// The user pasted input.
    Manual(String),
}

/// Waits for the browser callback or for pasted input, whichever comes first.
/// Without a server only the prompt is used. Port of
/// `waitForCallbackOrManualInput`.
pub(crate) async fn callback_or_manual<T: Send + 'static>(
    interaction: &Interaction,
    server: Option<&mut Server<T>>,
    message: &str,
    placeholder: &str,
) -> Result<Received<T>, AuthError> {
    let withdraw = interaction.cancel().child_token();
    let _withdraw_on_return = withdraw.clone().drop_guard();
    let manual = interaction.prompt_until(
        AuthPrompt::ManualCode {
            message: message.to_owned(),
            placeholder: Some(placeholder.to_owned()),
        },
        withdraw,
    );
    let Some(server) = server else {
        return manual.await.map(Received::Manual);
    };
    tokio::select! {
        value = server.wait() => value.map(Received::Callback),
        input = manual => input.map(Received::Manual),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthRequest;

    async fn get(url: &str) -> (u16, String) {
        let response = reqwest::Client::new().get(url).send().await.unwrap();
        (response.status().as_u16(), response.text().await.unwrap())
    }

    #[tokio::test]
    async fn answers_like_pi_and_yields_the_code() {
        let cancel = CancellationToken::new();
        let mut server = start_code_server(
            "Example",
            "127.0.0.1",
            0,
            "/callback",
            Some("s1".into()),
            &cancel,
        )
        .await
        .unwrap();
        let base = server.redirect_uri().to_owned();
        assert!(base.starts_with("http://127.0.0.1:") && base.ends_with("/callback"));
        let root = base.trim_end_matches("/callback");
        assert_eq!(get(&format!("{root}/other")).await.0, 404);
        assert_eq!(get(&format!("{base}?code=c&state=bad")).await.0, 400);
        assert_eq!(get(&format!("{base}?state=s1")).await.0, 400);
        let (status, body) = get(&format!("{base}?code=abc&state=s1")).await;
        assert_eq!(status, 200);
        assert!(body.contains("Signed in to Example. You may now close this page."));
        assert_eq!(server.wait().await, Ok("abc".to_owned()));
    }

    #[tokio::test]
    async fn pasted_input_wins_when_first() {
        let cancel = CancellationToken::new();
        let (interaction, mut requests) = Interaction::new(cancel.clone());
        let mut server = start_code_server("Example", "127.0.0.1", 0, "/cb", None, &cancel)
            .await
            .unwrap();
        let answer = tokio::spawn(async move {
            let Some(AuthRequest::Prompt { reply, .. }) = requests.recv().await else {
                panic!("expected a prompt");
            };
            reply.send("pasted".into()).unwrap();
        });
        let received = callback_or_manual(&interaction, Some(&mut server), "Paste", "x")
            .await
            .unwrap();
        answer.await.unwrap();
        assert_eq!(received, Received::Manual("pasted".into()));
    }
}
