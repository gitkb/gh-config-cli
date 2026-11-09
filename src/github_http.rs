//! Serialized GitHub requests with write pacing and bounded rate-limit recovery.

use crate::error::{AppError, AppResult};
use log::{debug, warn};
use reqwest::{header::HeaderMap, Client, Method, Response, StatusCode};
use serde_json::Value;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::{
    sync::Mutex,
    time::{sleep, sleep_until, Instant},
};

const WRITE_INTERVAL: Duration = Duration::from_secs(1);
const MAX_RETRIES: u32 = 3;

pub(crate) struct GitHubHttp {
    client: Client,
    token: String,
    // Held through retries so another request cannot bypass a rate-limit cooldown.
    last_write: Mutex<Option<Instant>>,
}

impl GitHubHttp {
    pub(crate) fn new(token: &str) -> Self {
        Self {
            client: Client::new(),
            token: token.into(),
            last_write: Mutex::new(None),
        }
    }

    pub(crate) async fn request(
        &self,
        method: Method,
        url: &str,
        accept: &str,
        body: Option<Value>,
    ) -> AppResult<Response> {
        if url.trim().is_empty()
            || self.token.trim().is_empty()
            || self.token.contains(['\r', '\n'])
        {
            return Err(AppError::GitHubApi(
                "A nonempty URL and valid GitHub token are required".into(),
            ));
        }
        let mut last_write = self.last_write.lock().await;
        let writes = !matches!(method, Method::GET | Method::HEAD | Method::OPTIONS);
        for attempt in 0..=MAX_RETRIES {
            if writes {
                if let Some(last) = *last_write {
                    sleep_until(last + WRITE_INTERVAL).await;
                }
            }
            let mut request = self
                .client
                .request(method.clone(), url)
                .bearer_auth(&self.token)
                .header("Accept", accept)
                .header("User-Agent", "gh-config");
            if let Some(body) = &body {
                request = request.json(body);
            }
            // Count failed/ambiguous writes too, including if this future is cancelled.
            if writes {
                *last_write = Some(Instant::now());
            }
            let response = request.send().await?;
            if writes {
                *last_write = Some(Instant::now());
            }
            let status = response.status();
            debug!("{method} {url} returned {status}");
            if status.is_success() {
                return Ok(response);
            }
            let headers = response.headers().clone();
            let text = response.text().await?;
            let delay = rate_limit_delay(status, &headers, &text, attempt, SystemTime::now());
            if attempt < MAX_RETRIES {
                if let Some(delay) = delay {
                    warn!(
                        "{method} {url} rate limited ({status}); retry {}/{} in {}s",
                        attempt + 1,
                        MAX_RETRIES,
                        delay.as_secs()
                    );
                    sleep(delay).await;
                    continue;
                }
            }
            // Include the status even when GitHub's body is just HTML or 'Not Found'.
            let exhausted = if delay.is_some() {
                " after exhausting rate-limit retries"
            } else {
                ""
            };
            return Err(AppError::GitHubResponse {
                status,
                message: format!("{method} {url} failed with {status}{exhausted}: {text}"),
            });
        }
        Err(AppError::GitHubApi(
            "Rate-limit retry budget exhausted".into(),
        ))
    }
}

fn rate_limit_delay(
    status: StatusCode,
    headers: &HeaderMap,
    body: &str,
    attempt: u32,
    now: SystemTime,
) -> Option<Duration> {
    let number = |name: &str| headers.get(name)?.to_str().ok()?.parse::<u64>().ok();
    let remaining = number("x-ratelimit-remaining");
    let body = body.to_ascii_lowercase();
    let limited = status == StatusCode::TOO_MANY_REQUESTS
        || (status == StatusCode::FORBIDDEN
            && (remaining == Some(0)
                || headers.contains_key("retry-after")
                || body.contains("rate limit")
                || body.contains("abuse detection")));
    if !limited {
        return None;
    }
    if let Some(seconds) = number("retry-after") {
        return Some(Duration::from_secs(seconds));
    }
    if remaining == Some(0) {
        if let Some(reset) = number("x-ratelimit-reset") {
            let current = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
            return Some(Duration::from_secs(
                reset.saturating_sub(current).saturating_add(1),
            ));
        }
    }
    Some(Duration::from_secs(60 * 2_u64.pow(attempt)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        task::JoinHandle,
    };

    type Requests = JoinHandle<Vec<(Instant, String)>>;

    // A small scripted HTTP peer lets each retry receive a different response.
    async fn server(responses: Vec<(u16, &'static str, &'static str)>) -> (String, Requests) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("address"));
        let requests = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (status, headers, body) in responses {
                let (mut socket, _) = listener.accept().await.expect("accept");
                let mut request = Vec::new();
                loop {
                    let mut buffer = [0; 4096];
                    let n = socket.read(&mut buffer).await.expect("read");
                    assert_ne!(n, 0, "request closed early");
                    request.extend_from_slice(&buffer[..n]);
                    if let Some(end) = request.windows(4).position(|s| s == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                        let length = head
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length: "))
                            .map(|v| v.parse::<usize>().expect("content length"))
                            .unwrap_or(0);
                        if request.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                requests.push((
                    Instant::now(),
                    String::from_utf8(request).expect("request text"),
                ));
                let response = format!("HTTP/1.1 {status} Test\r\nConnection: close\r\nContent-Length: {}\r\n{headers}\r\n{body}", body.len());
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("respond");
            }
            requests
        });
        (url, requests)
    }

    #[test]
    fn cooldown_uses_headers_and_exponential_fallback_without_retrying_permissions_errors() {
        let now = UNIX_EPOCH + Duration::from_secs(1000);
        let mut headers = HeaderMap::new();
        for (attempt, seconds) in [(0, 60), (1, 120), (2, 240)] {
            assert_eq!(
                rate_limit_delay(
                    StatusCode::TOO_MANY_REQUESTS,
                    &headers,
                    "<html>Too many requests</html>",
                    attempt,
                    now
                ),
                Some(Duration::from_secs(seconds))
            );
        }
        assert_eq!(
            rate_limit_delay(
                StatusCode::FORBIDDEN,
                &headers,
                "You have exceeded a secondary rate limit.",
                0,
                now
            ),
            Some(Duration::from_secs(60))
        );
        assert_eq!(
            rate_limit_delay(
                StatusCode::FORBIDDEN,
                &headers,
                "Resource not accessible by integration",
                0,
                now
            ),
            None
        );
        assert_eq!(
            rate_limit_delay(
                StatusCode::UNAUTHORIZED,
                &headers,
                "Bad credentials",
                0,
                now
            ),
            None
        );
        headers.insert("x-ratelimit-remaining", "0".parse().expect("header"));
        headers.insert("x-ratelimit-reset", "1100".parse().expect("header"));
        assert_eq!(
            rate_limit_delay(StatusCode::FORBIDDEN, &headers, "", 0, now),
            Some(Duration::from_secs(101))
        );
        headers.insert("retry-after", "7".parse().expect("header"));
        assert_eq!(
            rate_limit_delay(StatusCode::TOO_MANY_REQUESTS, &headers, "", 0, now),
            Some(Duration::from_secs(7))
        );
        headers.insert("retry-after", "bad".parse().expect("header"));
        assert_eq!(
            rate_limit_delay(StatusCode::FORBIDDEN, &headers, "", 0, now),
            Some(Duration::from_secs(101))
        );
        headers.insert("x-ratelimit-reset", "999".parse().expect("header"));
        assert_eq!(
            rate_limit_delay(StatusCode::FORBIDDEN, &headers, "", 0, now),
            Some(Duration::from_secs(1))
        );
    }

    #[tokio::test]
    async fn every_method_recovers_from_throttling_and_preserves_the_request() {
        for method in [
            Method::GET,
            Method::POST,
            Method::PATCH,
            Method::PUT,
            Method::DELETE,
        ] {
            let (url, requests) = server(vec![
                (
                    429,
                    "Retry-After: 0\r\n",
                    "<html>secondary rate limit</html>",
                ),
                (200, "", "{}"),
            ])
            .await;
            let client = GitHubHttp::new("test-token");
            client
                .request(
                    method.clone(),
                    &url,
                    "application/vnd.github.v3.repository+json",
                    Some(json!({"permission": "push"})),
                )
                .await
                .expect("retry recovers");
            let requests = requests.await.expect("server");
            assert_eq!(requests.len(), 2);
            assert_eq!(
                requests[0].1, requests[1].1,
                "retry changed method, headers or body for {method}"
            );
            assert!(requests[0].1.starts_with(method.as_str()));
            assert!(requests[0]
                .1
                .contains("application/vnd.github.v3.repository+json"));
            assert!(requests[0].1.contains("Bearer test-token"));
            assert!(requests[0].1.ends_with(r#"{"permission":"push"}"#));
            if method != Method::GET {
                assert!(requests[1].0.duration_since(requests[0].0) >= WRITE_INTERVAL);
            }
        }
    }

    #[tokio::test]
    async fn retry_after_is_observed_for_reads() {
        let (url, requests) = server(vec![
            (403, "Retry-After: 1\r\n", "limited"),
            (200, "", "{}"),
        ])
        .await;
        GitHubHttp::new("t")
            .request(Method::GET, &url, "application/vnd.github+json", None)
            .await
            .expect("retry");
        let requests = requests.await.expect("server");
        assert!(requests[1].0.duration_since(requests[0].0) >= Duration::from_secs(1));
    }

    #[tokio::test]
    async fn retries_stop_at_the_budget_and_ordinary_errors_fail_immediately() {
        let (url, requests) = server(vec![(429, "Retry-After: 0\r\n", "limited"); 4]).await;
        let err = GitHubHttp::new("t")
            .request(Method::GET, &url, "application/vnd.github+json", None)
            .await
            .expect_err("must stop");
        assert!(
            err.to_string().contains("429") && err.to_string().contains("exhausting"),
            "{err}"
        );
        assert_eq!(requests.await.expect("server").len(), 4);
        for status in [401, 403, 404, 422, 500] {
            let (url, requests) = server(vec![(status, "", "rejected")]).await;
            let err = GitHubHttp::new("t")
                .request(Method::PUT, &url, "application/vnd.github+json", None)
                .await
                .expect_err("must fail");
            assert!(err.to_string().contains(&status.to_string()), "{err}");
            assert_eq!(requests.await.expect("server").len(), 1);
        }
    }

    #[tokio::test]
    async fn another_request_cannot_bypass_a_rate_limit_cooldown() {
        let (url, requests) = server(vec![
            (429, "Retry-After: 1\r\n", "limited"),
            (200, "", "{}"),
            (200, "", "{}"),
        ])
        .await;
        let client = GitHubHttp::new("t");
        let limited = format!("{url}/limited");
        let other = format!("{url}/other");
        let (a, b) = tokio::join!(
            client.request(Method::GET, &limited, "application/vnd.github+json", None),
            client.request(Method::GET, &other, "application/vnd.github+json", None),
        );
        a.expect("limited request recovers");
        b.expect("other request waits");
        let requests = requests.await.expect("server");
        assert!(requests[0].1.starts_with("GET /limited "));
        assert!(requests[1].1.starts_with("GET /limited "));
        assert!(requests[2].1.starts_with("GET /other "));
        assert!(requests[1].0.duration_since(requests[0].0) >= Duration::from_secs(1));
    }

    #[tokio::test]
    async fn concurrent_writes_share_one_pacing_gate_across_methods() {
        let (url, requests) = server(vec![(200, "", "{}"); 3]).await;
        let client = GitHubHttp::new("t");
        let (a, b, c) = tokio::join!(
            client.request(Method::POST, &url, "application/vnd.github+json", None),
            client.request(Method::PATCH, &url, "application/vnd.github+json", None),
            client.request(Method::DELETE, &url, "application/vnd.github+json", None),
        );
        a.expect("post");
        b.expect("patch");
        c.expect("delete");
        let requests = requests.await.expect("server");
        for pair in requests.windows(2) {
            assert!(pair[1].0.duration_since(pair[0].0) >= WRITE_INTERVAL);
        }
    }
}
