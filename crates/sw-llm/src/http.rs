//! Shared HTTP plumbing for the adapters: sending with retries, classifying error
//! responses, and reading a response body as server-sent events.

use std::time::Duration;

use futures_util::{Stream, StreamExt, stream};
use rand::RngExt;
use reqwest::StatusCode;

use crate::error::LlmError;
use crate::sse::{SseEvent, SseParser};
use crate::{Event, EventStream};

/// Longest we are willing to wait before a retry, whatever the provider asks for.
const MAX_BACKOFF: Duration = Duration::from_secs(60);
const BASE_BACKOFF: Duration = Duration::from_millis(500);

/// Build the HTTP client used for one provider.
pub fn client(timeout: Duration) -> Result<reqwest::Client, LlmError> {
    sw_core::crypto::install_default_provider();
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15).min(timeout))
        // Applies to each read, so a long generation is fine but a stalled one is not.
        .read_timeout(timeout)
        .user_agent(concat!("sitewright/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| LlmError::Config(format!("cannot build the HTTP client: {e}")))
}

/// Send the request built by `build`, retrying rate-limit, server and network errors
/// with exponential backoff and jitter (or the provider's `Retry-After`). Returns the
/// successful response, or the last error once `max_retries` retries are used up.
pub async fn send_with_retries<F>(
    max_retries: u32,
    timeout: Duration,
    build: F,
) -> Result<reqwest::Response, LlmError>
where
    F: Fn() -> reqwest::RequestBuilder,
{
    let mut attempt = 0;
    loop {
        let error = match build().send().await {
            Ok(response) if response.status().is_success() => return Ok(response),
            Ok(response) => classify(response).await,
            Err(e) if e.is_timeout() => LlmError::Timeout(timeout),
            Err(e) => LlmError::Network(describe(&e)),
        };
        if !error.is_retryable() || attempt >= max_retries {
            return Err(error);
        }
        let wait = backoff(attempt, &error);
        tracing::warn!(%error, attempt = attempt + 1, max_retries, wait_ms = wait.as_millis() as u64, "model call failed; retrying");
        tokio::time::sleep(wait).await;
        attempt += 1;
    }
}

/// How long to wait before retry number `attempt + 1`.
fn backoff(attempt: u32, error: &LlmError) -> Duration {
    if let LlmError::RateLimited {
        retry_after: Some(wait),
        ..
    } = error
    {
        return (*wait).min(MAX_BACKOFF);
    }
    let exponential = BASE_BACKOFF
        .saturating_mul(2u32.saturating_pow(attempt))
        .min(MAX_BACKOFF);
    // Full jitter on the upper half, so parallel runs do not retry in step.
    let half = exponential / 2;
    half + half.mul_f64(rand::rng().random::<f64>())
}

/// Turn a non-success response into the matching error.
async fn classify(response: reqwest::Response) -> LlmError {
    let status = response.status();
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|secs| secs.is_finite() && *secs >= 0.0)
        .map(Duration::from_secs_f64);
    let body = response.text().await.unwrap_or_default();
    let message = error_message(&body);
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => LlmError::Auth {
            status: status.as_u16(),
            message,
        },
        StatusCode::TOO_MANY_REQUESTS => LlmError::RateLimited {
            message,
            retry_after,
        },
        s if s.is_server_error() => LlmError::Server {
            status: s.as_u16(),
            message,
        },
        s => LlmError::BadRequest {
            status: s.as_u16(),
            message,
        },
    }
}

/// Pull the human-readable message out of an error body. Providers use
/// `{"error": {"message": ...}}`, `{"error": "..."}` or `{"message": ...}`.
fn error_message(body: &str) -> String {
    let parsed: Option<serde_json::Value> = serde_json::from_str(body).ok();
    let message = parsed.as_ref().and_then(|v| {
        v.pointer("/error/message")
            .or_else(|| v.get("error").filter(|e| e.is_string()))
            .or_else(|| v.get("message"))
            .and_then(|m| m.as_str())
    });
    let text = message.unwrap_or(body).trim();
    let mut short: String = text.chars().take(500).collect();
    if short.is_empty() {
        short.push_str("(no details)");
    }
    short
}

fn describe(error: &reqwest::Error) -> String {
    // reqwest's Display includes the URL, which for some providers carries the key, so
    // describe the error by its kind and its causes instead.
    let mut text = String::from(if error.is_connect() {
        "connection failed"
    } else if error.is_body() || error.is_decode() {
        "the response was interrupted"
    } else {
        "the request failed"
    });
    let mut source = std::error::Error::source(error);
    while let Some(inner) = source {
        text.push_str(": ");
        text.push_str(&inner.to_string());
        source = inner.source();
    }
    text
}

/// Read a streaming response body as server-sent events.
pub fn sse_events(
    response: reqwest::Response,
    timeout: Duration,
) -> impl Stream<Item = Result<SseEvent, LlmError>> + Send {
    struct State<B> {
        body: B,
        parser: SseParser,
        done: bool,
    }
    let state = State {
        body: Box::pin(response.bytes_stream()),
        parser: SseParser::new(),
        done: false,
    };
    stream::unfold(state, move |mut state| async move {
        if state.done {
            return None;
        }
        loop {
            let batch: Vec<Result<SseEvent, LlmError>> = match state.body.next().await {
                Some(Ok(chunk)) => state.parser.feed(&chunk).into_iter().map(Ok).collect(),
                Some(Err(e)) => {
                    state.done = true;
                    vec![Err(if e.is_timeout() {
                        LlmError::Timeout(timeout)
                    } else {
                        LlmError::Network(describe(&e))
                    })]
                }
                None => {
                    state.done = true;
                    let last: Vec<_> = state.parser.finish().map(Ok).into_iter().collect();
                    if last.is_empty() {
                        return None;
                    }
                    last
                }
            };
            if !batch.is_empty() {
                return Some((stream::iter(batch), state));
            }
        }
    })
    .flatten()
}

/// Translate a provider's server-sent events into normalized [`Event`]s.
///
/// `step` is called with each SSE event and finally with `None` when the body ends; it
/// returns the events to emit. The stream stops after the first error, and after `step`
/// has seen `None`.
pub fn translate<S, St, F>(events: S, state: St, step: F) -> EventStream
where
    S: Stream<Item = Result<SseEvent, LlmError>> + Send + 'static,
    St: Send + 'static,
    F: FnMut(&mut St, Option<SseEvent>) -> Result<Vec<Event>, LlmError> + Send + 'static,
{
    struct State<S, St, F> {
        events: std::pin::Pin<Box<S>>,
        state: St,
        step: F,
        done: bool,
    }
    let state = State {
        events: Box::pin(events),
        state,
        step,
        done: false,
    };
    Box::pin(
        stream::unfold(state, |mut s| async move {
            if s.done {
                return None;
            }
            loop {
                let result = match s.events.next().await {
                    Some(Ok(event)) => (s.step)(&mut s.state, Some(event)),
                    Some(Err(e)) => Err(e),
                    None => {
                        s.done = true;
                        (s.step)(&mut s.state, None)
                    }
                };
                let batch: Vec<Result<Event, LlmError>> = match result {
                    Ok(events) => events.into_iter().map(Ok).collect(),
                    Err(e) => {
                        s.done = true;
                        vec![Err(e)]
                    }
                };
                if !batch.is_empty() {
                    return Some((stream::iter(batch), s));
                }
                if s.done {
                    return None;
                }
            }
        })
        .flatten(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_error_messages() {
        assert_eq!(
            error_message(r#"{"error":{"message":"bad key","code":"401"}}"#),
            "bad key"
        );
        assert_eq!(error_message(r#"{"error":"nope"}"#), "nope");
        assert_eq!(error_message(r#"{"message":"hi"}"#), "hi");
        assert_eq!(error_message("plain text"), "plain text");
        assert_eq!(error_message(""), "(no details)");
        assert_eq!(error_message(&"x".repeat(2000)).len(), 500);
    }

    #[test]
    fn backoff_grows_and_honours_retry_after() {
        let server = LlmError::Server {
            status: 500,
            message: String::new(),
        };
        for attempt in 0..10 {
            let wait = backoff(attempt, &server);
            let full = BASE_BACKOFF
                .saturating_mul(2u32.saturating_pow(attempt))
                .min(MAX_BACKOFF);
            assert!(
                wait >= full / 2 && wait <= full,
                "attempt {attempt}: {wait:?}"
            );
        }
        let limited = LlmError::RateLimited {
            message: String::new(),
            retry_after: Some(Duration::from_secs(7)),
        };
        assert_eq!(backoff(0, &limited), Duration::from_secs(7));
        let greedy = LlmError::RateLimited {
            message: String::new(),
            retry_after: Some(Duration::from_secs(9999)),
        };
        assert_eq!(backoff(0, &greedy), MAX_BACKOFF);
    }
}
