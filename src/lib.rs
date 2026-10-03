//! Solve Cloudflare Turnstile widgets with the ZeroCaptcha REST API: give it a page's URL and its
//! sitekey, and it returns a token to submit as the browser would.
//!
//! ```no_run
//! use std::time::Duration;
//! use cloudflare_turnstile_solver::{Client, Task};
//!
//! let client = Client::new(std::env::var("ZEROCAPTCHA_API")?, std::env::var("ZEROCAPTCHA_KEY")?);
//! let token = client.solve(
//!     &Task {
//!         // The widget's data-action and data-cdata, or turnstile.render()'s action and cData.
//!         action: Some("login"),
//!         cdata: Some("session-7f3a9c2e"),
//!         ..Task::new("https://shop.example.com/login", "0x4AAAAAAAB1cD2eF3gH4iJ5")
//!     },
//!     Duration::from_secs(180),
//! )?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::fmt;
use std::thread::sleep;
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

/// What to solve: the page the widget is on and its sitekey, as the widget declares them.
#[derive(Debug, Clone, Default)]
pub struct Task<'a> {
    pub website_url: &'a str,
    pub website_key: &'a str,
    /// The widget's `data-action`, if it sets one.
    pub action: Option<&'a str>,
    /// The widget's `data-cdata`, if it sets one.
    pub cdata: Option<&'a str>,
    /// Your proxy, such as `http://user:pass@proxy.example.net:8080`, to solve through it.
    pub proxy: Option<&'a str>,
}

impl<'a> Task<'a> {
    /// A task for the widget with this sitekey on this page.
    pub fn new(website_url: &'a str, website_key: &'a str) -> Self {
        Task {
            website_url,
            website_key,
            ..Task::default()
        }
    }
}

/// A refusal from the API, a task that ended without a token, or a wait that ran out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    /// The API's code, such as `insufficient_funds` or `ERROR_CAPTCHA_UNSOLVABLE`, or `timeout`.
    pub code: String,
    pub message: String,
    /// What to quote when you ask support about the request.
    pub request_id: Option<String>,
}

impl Error {
    fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Error {
            code: code.into(),
            message: message.into(),
            request_id: None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)?;
        if let Some(id) = &self.request_id {
            write!(f, " (request {id})")?;
        }
        Ok(())
    }
}

impl std::error::Error for Error {}

/// Answers worth another try after a wait: too many requests, or a server busy or away.
const RETRYABLE: [u16; 4] = [429, 502, 503, 504];
const ATTEMPTS: u32 = 3;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The ZeroCaptcha API, as one account's key sees it.
pub struct Client {
    api: String,
    key: String,
    interval: Duration,
    agent: ureq::Agent,
}

impl Client {
    /// A client for the API at `api` (such as `https://api.zerocaptcha.io`) with your key, `zc_live_…`.
    pub fn new(api: impl Into<String>, key: impl Into<String>) -> Self {
        Client {
            api: api.into().trim_end_matches('/').to_owned(),
            key: key.into(),
            interval: Duration::from_secs(2),
            agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(10))
                .build(),
        }
    }

    /// Asks for the result this often instead of every 2 seconds.
    #[must_use]
    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    /// Creates a Cloudflare Turnstile task, waits for it, and returns the token. The token works
    /// once, for 300 seconds. A task that fails or expires is an [`Error`] with its errorCode, and
    /// nothing is charged; the wait never runs past `timeout`.
    pub fn solve(&self, task: &Task<'_>, timeout: Duration) -> Result<String, Error> {
        let deadline = Instant::now() + timeout;
        let mut body = Map::new();
        let kind = if task.proxy.is_some() {
            "TurnstileTask"
        } else {
            "TurnstileTaskProxyless"
        };
        body.insert("type".into(), json!(kind));
        body.insert("websiteURL".into(), json!(task.website_url));
        body.insert("websiteKey".into(), json!(task.website_key));
        for (field, value) in [
            ("action", task.action),
            ("cdata", task.cdata),
            ("proxy", task.proxy),
        ] {
            if let Some(value) = value {
                body.insert(field.into(), json!(value));
            }
        }
        // One key per task: a retry after a lost reply returns this task instead of making another.
        let idempotency_key = uuid::Uuid::new_v4().to_string();
        let body = Value::Object(body);
        let mut current = self.request(
            "POST",
            "/v1/tasks",
            deadline,
            Some(&body),
            Some(&idempotency_key),
        )?;
        while matches!(text(&current, "status"), Some("queued" | "running")) {
            let id = text(&current, "id").unwrap_or_default().to_owned();
            if Instant::now() + self.interval >= deadline {
                return Err(Error::new(
                    "timeout",
                    format!("task {id} was still running at the deadline"),
                ));
            }
            sleep(self.interval);
            current = self.request("GET", &format!("/v1/tasks/{id}"), deadline, None, None)?;
        }
        let status = text(&current, "status").unwrap_or("unknown").to_owned();
        let token = current
            .get("solution")
            .and_then(|solution| text(solution, "token"));
        match token {
            Some(token) if status == "succeeded" && !token.is_empty() => Ok(token.to_owned()),
            _ => Err(Error::new(
                text(&current, "errorCode").unwrap_or(&status),
                text(&current, "errorDescription").map_or_else(
                    || format!("the task {status}; nothing was charged"),
                    str::to_owned,
                ),
            )),
        }
    }

    /// Sends one request, trying it up to three times with the same Idempotency-Key.
    fn request(
        &self,
        method: &str,
        path: &str,
        deadline: Instant,
        body: Option<&Value>,
        idempotency_key: Option<&str>,
    ) -> Result<Value, Error> {
        let past_deadline =
            || Error::new("timeout", format!("{method} {path} ran past the deadline"));
        let mut attempt = 1;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(past_deadline());
            }
            let mut request = self
                .agent
                .request(method, &format!("{}{path}", self.api))
                .timeout(left.min(REQUEST_TIMEOUT))
                .set("Authorization", &format!("Bearer {}", self.key))
                .set("Accept", "application/json");
            if let Some(key) = idempotency_key {
                request = request.set("Idempotency-Key", key);
            }
            let result = match body {
                Some(body) => request
                    .set("Content-Type", "application/json")
                    .send_string(&body.to_string()),
                None => request.call(),
            };
            let mut wait = Duration::from_secs(u64::from(attempt));
            let failure = match result {
                Ok(response) => match response
                    .into_string()
                    .map(|text| serde_json::from_str::<Value>(&text))
                {
                    Ok(Ok(value)) if value.is_object() => return Ok(value),
                    // An answer cut short: the same Idempotency-Key makes a retry safe.
                    _ => Error::new("network", "the answer was cut short"),
                },
                Err(ureq::Error::Status(status, response)) => {
                    let retry_after = response
                        .header("Retry-After")
                        .and_then(|value| value.trim().parse::<u64>().ok());
                    let request_id = response.header("X-Request-Id").map(str::to_owned);
                    let refusal = problem(
                        status,
                        response.into_string().unwrap_or_default(),
                        request_id,
                    );
                    let retryable = RETRYABLE.contains(&status)
                        || (status == 409 && refusal.code == "idempotency_key_in_use");
                    if !retryable {
                        return Err(refusal);
                    }
                    if let Some(seconds) = retry_after {
                        wait = Duration::from_secs(seconds);
                    }
                    refusal
                }
                // No answer: the same Idempotency-Key makes a retry safe.
                Err(ureq::Error::Transport(transport)) => {
                    Error::new("network", transport.to_string())
                }
            };
            if attempt >= ATTEMPTS {
                return Err(failure);
            }
            if Instant::now() + wait >= deadline {
                return Err(past_deadline());
            }
            sleep(wait);
            attempt += 1;
        }
    }
}

/// The API's problem document (RFC 9457) as an error: its code, detail and request ID.
fn problem(status: u16, body: String, request_id: Option<String>) -> Error {
    let fields = serde_json::from_str::<Value>(&body).unwrap_or(Value::Null);
    Error {
        code: text(&fields, "code").map_or_else(|| format!("http_{status}"), str::to_owned),
        message: text(&fields, "detail")
            .or_else(|| text(&fields, "title"))
            .map_or_else(|| format!("HTTP {status}"), str::to_owned),
        request_id: text(&fields, "request_id")
            .map(str::to_owned)
            .or(request_id),
    }
}

/// A field's text, or `None` when it is missing or not text.
fn text<'v>(value: &'v Value, name: &str) -> Option<&'v str> {
    value.get(name).and_then(Value::as_str)
}
