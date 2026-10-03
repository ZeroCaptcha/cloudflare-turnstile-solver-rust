//! The solver against a stand-in API on this machine: no key, no real task, nothing spent.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use cloudflare_turnstile_solver::{Client, Task};
use serde_json::{json, Value};

const KEY: &str = "zc_live_test_key";
const TASK_ID: &str = "0192f3a4-7b1c-7d2e-9f10-3c4d5e6f7a8b";
const TOKEN: &str = "0.stand-in-turnstile-token";
const PAGE: &str = "https://shop.example.com/login";
const SITEKEY: &str = "0x4AAAAAAAB1cD2eF3gH4iJ5";

#[derive(Debug, Clone)]
struct Recorded {
    method: String,
    path: String,
    authorization: Option<String>,
    idempotency_key: Option<String>,
    body: Value,
}

#[derive(Default)]
struct State {
    requests: Vec<Recorded>,
    creates: u32,
    polls: u32,
}

/// A stand-in for the ZeroCaptcha REST API: POST /v1/tasks and GET /v1/tasks/{id}, playing one
/// scenario: "success", "failed", "rate-limited" (the first create is 429 with Retry-After: 0) or
/// "insufficient-funds". It answers one request per connection.
struct StandIn {
    url: String,
    state: Arc<Mutex<State>>,
}

impl StandIn {
    fn start(scenario: &'static str) -> StandIn {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(State::default()));
        let shared = Arc::clone(&state);
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                serve(stream, scenario, &shared);
            }
        });
        StandIn { url, state }
    }

    fn requests(&self) -> Vec<Recorded> {
        self.state.lock().unwrap().requests.clone()
    }
}

fn serve(mut stream: TcpStream, scenario: &str, state: &Mutex<State>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let path = parts.next().unwrap_or_default().to_owned();
    let (mut length, mut authorization, mut idempotency_key) = (0, None, None);
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).unwrap();
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        let (name, value) = header.split_once(':').unwrap();
        match name.to_ascii_lowercase().as_str() {
            "content-length" => length = value.trim().parse().unwrap(),
            "authorization" => authorization = Some(value.trim().to_owned()),
            "idempotency-key" => idempotency_key = Some(value.trim().to_owned()),
            _ => {}
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    let body = serde_json::from_slice(&body).unwrap_or(Value::Null);

    let mut state = state.lock().unwrap();
    state.requests.push(Recorded {
        method: method.clone(),
        path: path.clone(),
        authorization: authorization.clone(),
        idempotency_key,
        body,
    });
    let problem = |status: u16, code: &str| {
        (
            status,
            json!({ "type": "about:blank", "title": code, "status": status, "code": code, "detail": format!("The stand-in answered {code}."), "request_id": "0192f3a4-0000-7000-8000-000000000000" }),
        )
    };
    let mut retry_after = false;
    let (status, reply) = if authorization.as_deref() != Some(&format!("Bearer {KEY}")) {
        problem(401, "unauthorized")
    } else if method == "POST" && path == "/v1/tasks" {
        state.creates += 1;
        if scenario == "rate-limited" && state.creates == 1 {
            retry_after = true;
            problem(429, "rate_limited")
        } else if scenario == "insufficient-funds" {
            problem(402, "insufficient_funds")
        } else {
            (201, json!({ "id": TASK_ID, "status": "queued" }))
        }
    } else if method == "GET" && path == format!("/v1/tasks/{TASK_ID}") {
        state.polls += 1;
        if scenario == "failed" {
            (
                200,
                json!({ "id": TASK_ID, "status": "failed", "errorCode": "ERROR_CAPTCHA_UNSOLVABLE", "errorDescription": "Every attempt failed." }),
            )
        } else if state.polls == 1 {
            (200, json!({ "id": TASK_ID, "status": "running" }))
        } else {
            (
                200,
                json!({ "id": TASK_ID, "status": "succeeded", "solution": { "token": TOKEN } }),
            )
        }
    } else {
        problem(404, "not_found")
    };
    let text = reply.to_string();
    let extra = if retry_after {
        "Retry-After: 0\r\n"
    } else {
        ""
    };
    write!(
        stream,
        "HTTP/1.1 {status} Stand-in\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n{text}",
        text.len()
    )
    .unwrap();
}

fn client(api: &StandIn) -> Client {
    Client::new(api.url.clone(), KEY).with_interval(Duration::from_millis(10))
}

const TIMEOUT: Duration = Duration::from_secs(30);

#[test]
fn returns_the_token_and_sends_the_task_as_the_api_expects() {
    let api = StandIn::start("success");
    let task = Task {
        action: Some("login"),
        cdata: Some("session-7f3a9c2e"),
        ..Task::new(PAGE, SITEKEY)
    };
    assert_eq!(client(&api).solve(&task, TIMEOUT).unwrap(), TOKEN);
    let requests = api.requests();
    let create = &requests[0];
    assert_eq!(
        (create.method.as_str(), create.path.as_str()),
        ("POST", "/v1/tasks")
    );
    assert_eq!(
        create.authorization.as_deref(),
        Some("Bearer zc_live_test_key")
    );
    assert!(create.idempotency_key.is_some());
    // The widget's action and cData reach the API, so a site that checks them accepts the token.
    assert_eq!(
        create.body,
        json!({ "type": "TurnstileTaskProxyless", "websiteURL": PAGE, "websiteKey": SITEKEY, "action": "login", "cdata": "session-7f3a9c2e" })
    );
    assert_eq!(requests.len(), 3);
}

#[test]
fn a_proxy_makes_a_proxied_task() {
    let api = StandIn::start("success");
    let proxy = "http://user:pass@proxy.example.net:8080";
    client(&api)
        .solve(
            &Task {
                proxy: Some(proxy),
                ..Task::new(PAGE, SITEKEY)
            },
            TIMEOUT,
        )
        .unwrap();
    let body = &api.requests()[0].body;
    assert_eq!(body["type"], "TurnstileTask");
    assert_eq!(body["proxy"], proxy);
}

#[test]
fn a_failed_task_is_its_code() {
    let api = StandIn::start("failed");
    let error = client(&api)
        .solve(&Task::new(PAGE, SITEKEY), TIMEOUT)
        .unwrap_err();
    assert_eq!(error.code, "ERROR_CAPTCHA_UNSOLVABLE");
}

#[test]
fn a_rate_limited_create_is_retried_with_the_same_key() {
    let api = StandIn::start("rate-limited");
    assert_eq!(
        client(&api)
            .solve(&Task::new(PAGE, SITEKEY), TIMEOUT)
            .unwrap(),
        TOKEN
    );
    let requests = api.requests();
    assert_eq!(
        (requests[0].method.as_str(), requests[1].method.as_str()),
        ("POST", "POST")
    );
    assert_eq!(requests[0].idempotency_key, requests[1].idempotency_key);
}

#[test]
fn a_refusal_is_returned_at_once_with_its_request_id() {
    let api = StandIn::start("insufficient-funds");
    let error = client(&api)
        .solve(&Task::new(PAGE, SITEKEY), TIMEOUT)
        .unwrap_err();
    assert_eq!(error.code, "insufficient_funds");
    assert!(error.request_id.is_some());
    assert_eq!(api.requests().len(), 1);
}

#[test]
fn the_deadline_stops_the_wait() {
    let api = StandIn::start("success");
    let slow = Client::new(api.url.clone(), KEY).with_interval(Duration::from_secs(1));
    let error = slow
        .solve(&Task::new(PAGE, SITEKEY), Duration::from_millis(300))
        .unwrap_err();
    assert_eq!(error.code, "timeout");
}
