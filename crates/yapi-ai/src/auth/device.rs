//! RFC 8628 device authorization polling. Port of `oauth/device-code.ts`.

use std::future::Future;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::{AuthError, form};

/// RFC 8628's grant type for exchanging a device code.
pub(crate) const DEVICE_CODE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

const TIMEOUT_MESSAGE: &str = "Device flow timed out";
const SLOW_DOWN_TIMEOUT_MESSAGE: &str = "Device flow timed out after one or more slow_down responses. This is often caused by clock drift in WSL or VM environments. Please sync or restart the VM clock and try again.";
const MINIMUM_INTERVAL: Duration = Duration::from_secs(1);
/// RFC 8628 section 3.2: the default when the server omits `interval`.
const DEFAULT_INTERVAL_SECONDS: f64 = 5.0;
/// RFC 8628 section 3.5: `slow_down` adds five seconds.
const SLOW_DOWN_INCREMENT: Duration = Duration::from_secs(5);

/// An http(s) URL, the only kind opened in the browser.
pub(crate) fn trusted_url(value: &Value) -> Option<String> {
    let url = url::Url::parse(value.as_str()?).ok()?;
    matches!(url.scheme(), "http" | "https").then(|| url.to_string())
}

/// A finite number above zero.
pub(crate) fn positive(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .filter(|value| value.is_finite() && *value > 0.0)
}

/// A form POST that accepts JSON, with a 30-second timeout.
pub(crate) fn post_form(url: &str, fields: &[(&str, &str)]) -> reqwest::RequestBuilder {
    crate::http::client()
        .post(url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .timeout(Duration::from_secs(30))
        .body(form(fields))
}

/// The result of one poll.
#[derive(Debug, PartialEq)]
pub(crate) enum Poll<T> {
    /// Not yet authorized.
    Pending,
    /// Poll less often; the server may name the new interval.
    SlowDown(Option<f64>),
    /// The flow failed with this message.
    Failed(String),
    /// Authorized.
    Complete(T),
}

fn seconds(value: f64) -> Duration {
    Duration::try_from_secs_f64(value.max(0.0)).unwrap_or(Duration::MAX)
}

async fn sleep(duration: Duration, cancel: &CancellationToken) -> Result<(), AuthError> {
    tokio::select! {
        () = tokio::time::sleep(duration) => Ok(()),
        () = cancel.cancelled() => Err(AuthError::Cancelled),
    }
}

/// Polls until `poll` completes, fails, the code expires or `cancel` fires.
pub(crate) async fn poll_device_code<T, F, Fut>(
    interval_seconds: Option<f64>,
    expires_in_seconds: Option<f64>,
    wait_before_first_poll: bool,
    cancel: &CancellationToken,
    mut poll: F,
) -> Result<T, AuthError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Poll<T>, AuthError>>,
{
    let deadline = expires_in_seconds.map(|expires| Instant::now() + seconds(expires));
    let remaining = || {
        deadline.map_or(Duration::MAX, |deadline| {
            deadline.saturating_duration_since(Instant::now())
        })
    };
    let mut interval =
        seconds(interval_seconds.unwrap_or(DEFAULT_INTERVAL_SECONDS)).max(MINIMUM_INTERVAL);
    let mut slow_downs = 0;
    if wait_before_first_poll && !remaining().is_zero() {
        sleep(interval.min(remaining()), cancel).await?;
    }
    while !remaining().is_zero() {
        if cancel.is_cancelled() {
            return Err(AuthError::Cancelled);
        }
        match poll().await? {
            Poll::Complete(value) => return Ok(value),
            Poll::Failed(message) => return Err(AuthError::Failed(message)),
            Poll::SlowDown(server_interval) => {
                slow_downs += 1;
                interval = match server_interval {
                    Some(value) if value.is_finite() && value > 0.0 => seconds(value),
                    _ => interval + SLOW_DOWN_INCREMENT,
                }
                .max(MINIMUM_INTERVAL);
            }
            Poll::Pending => {}
        }
        if remaining().is_zero() {
            break;
        }
        sleep(interval.min(remaining()), cancel).await?;
    }
    Err(AuthError::failed(if slow_downs > 0 {
        SLOW_DOWN_TIMEOUT_MESSAGE
    } else {
        TIMEOUT_MESSAGE
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn polls_until_complete_and_honors_slow_down() {
        let cancel = CancellationToken::new();
        let mut calls = 0;
        let started = tokio::time::Instant::now();
        let value = poll_device_code(Some(1.0), Some(60.0), true, &cancel, || {
            calls += 1;
            let result = match calls {
                1 => Poll::Pending,
                2 => Poll::SlowDown(None),
                _ => Poll::Complete("token"),
            };
            async move { Ok(result) }
        })
        .await
        .unwrap();
        assert_eq!(value, "token");
        // 1 s before the first poll, 1 s after pending, 6 s after slow_down.
        assert_eq!(started.elapsed().as_secs(), 8);
    }

    #[tokio::test(start_paused = true)]
    async fn times_out_with_pi_messages() {
        let cancel = CancellationToken::new();
        let result: Result<(), _> =
            poll_device_code(Some(1.0), Some(3.0), false, &cancel, || async {
                Ok(Poll::SlowDown(Some(2.0)))
            })
            .await;
        assert_eq!(result, Err(AuthError::failed(SLOW_DOWN_TIMEOUT_MESSAGE)));
        let failed: Result<(), _> = poll_device_code(None, None, false, &cancel, || async {
            Ok(Poll::Failed("Device flow failed: access_denied".into()))
        })
        .await;
        assert_eq!(
            failed,
            Err(AuthError::failed("Device flow failed: access_denied"))
        );
    }
}
