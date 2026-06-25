//! Browser-based `difflore cloud login` flow.
//!
//! On `difflore cloud login` (no `--token`), we:
//!
//!   1. Spin up a one-shot HTTP server on `127.0.0.1:0` (random free port).
//!   2. Generate a cryptographic `state` nonce.
//!   3. Open the user's browser at
//!      `<cloud>/cli-auth?redirect_uri=...&state=...`.
//!   4. Wait for a single `/callback` request carrying `token`/`state`,
//!      verify state, and hand the token back to `main.rs`.
//!
//! The secret is read from the **POST body** when the browser submits one
//! (`Content-Type: application/x-www-form-urlencoded`), which keeps the bearer
//! and refresh token out of the request line — and therefore out of any
//! loopback request-line logging. A `GET ...?token=...` query is still accepted
//! as a fallback for clients that can only navigate, but the POST body is
//! preferred for confidentiality of the secret in transit on loopback.
//!
//! Hard timeout: 120 s. After success we hold the listener for ~500 ms so
//! the browser tab can render the "you can close this tab" page, then
//! shut down. No second request is ever processed.
//!
//! Failure modes (state mismatch, user-cancel, timeout) all bubble up as
//! `Err(String)` so the caller can fall back to the manual `--token` path.

use std::io::Cursor;
use std::sync::mpsc;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use rand::RngExt;
use tiny_http::{Header, Response, Server};

const FLOW_TIMEOUT_SECS: u64 = 120;
const POST_SUCCESS_LINGER_MS: u64 = 500;

pub struct BrowserLoginResult {
    pub token: String,
    pub refresh_token: Option<String>,
}

#[derive(Default)]
struct CallbackQuery {
    token: Option<String>,
    refresh_token: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

impl CallbackQuery {
    /// Overlay `other` (the POST body) onto `self` (the query string): any
    /// field present in the body wins, so the POST-delivered secret is
    /// preferred while a query fallback still populates absent fields.
    fn merge(self, other: Self) -> Self {
        Self {
            token: other.token.or(self.token),
            refresh_token: other.refresh_token.or(self.refresh_token),
            state: other.state.or(self.state),
            error: other.error.or(self.error),
        }
    }
}

/// Read a form-urlencoded request body (used for the preferred POST callback).
/// Bounded read so a hostile loopback client can't exhaust memory; non-form
/// requests (e.g. a plain GET navigation) yield an empty string and fall back
/// to the query parser. Errors are swallowed — a missing/oversized body simply
/// means no fields are contributed from the body.
fn read_form_body(req: &mut tiny_http::Request) -> String {
    use std::io::Read as _;

    let is_form = req
        .headers()
        .iter()
        .any(|h| h.field.equiv("Content-Type") && h.value.as_str().contains("urlencoded"));
    if !is_form {
        return String::new();
    }

    // Tokens are small; 64 KiB is generous and caps untrusted input.
    const MAX_BODY: u64 = 64 * 1024;
    let mut buf = String::new();
    let _ = req.as_reader().take(MAX_BODY).read_to_string(&mut buf);
    buf
}

fn parse_callback_query(qs: &str) -> CallbackQuery {
    let mut parsed = CallbackQuery::default();
    for pair in qs.split('&') {
        if pair.is_empty() {
            continue;
        }
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        let value = urldecode(v);
        match k {
            "token" => parsed.token = Some(value),
            "refreshToken" => parsed.refresh_token = Some(value),
            "state" => parsed.state = Some(value),
            "error" => parsed.error = Some(value),
            _ => {}
        }
    }
    parsed
}

/// Outcome of handling a single inbound HTTP request on the callback server.
///
/// The HTTP response has already been written for every variant; the caller
/// only decides whether to keep listening or terminate the flow.
enum CallbackOutcome {
    /// Non-terminal request (404 probe, missing state, missing token). Keep
    /// listening for the real `/callback` hit.
    KeepListening,
    /// Successful callback — the browser was told to close its tab.
    Token(BrowserLoginResult),
    /// Terminal failure (server-reported error or state mismatch). The flow
    /// is over and the message should bubble up to the caller.
    Failed(String),
}

/// Handle one inbound request on the callback server.
///
/// Parses the query, validates the `state` nonce, and extracts the token,
/// writing the appropriate HTTP response before returning. This is the single
/// per-request code path shared by the production loop and the test harness so
/// tests drive the same logic the browser flow runs.
fn handle_callback_request(mut req: tiny_http::Request, expected_state: &str) -> CallbackOutcome {
    let url = req.url().to_owned();
    if !url.starts_with("/callback") {
        let _ = req.respond(html_response(404, "<h1>404</h1>"));
        return CallbackOutcome::KeepListening;
    }

    // Prefer the POST body (form-encoded) so the secret never appears in the
    // request line; fall back to the query string for navigation-only clients.
    // The body must be read before `req.respond` consumes the request.
    let body = read_form_body(&mut req);
    let qs = url.split_once('?').map_or("", |(_, q)| q);
    let parsed = parse_callback_query(qs).merge(parse_callback_query(&body));

    if let Some(e) = parsed.error {
        let body = format!(
            "<h1>Login cancelled</h1><p>{}</p><p>You can close this tab.</p>",
            htmlescape(&e)
        );
        let _ = req.respond(html_response(200, &body));
        return CallbackOutcome::Failed(format!("Authorization failed: {e}"));
    }

    let Some(got) = parsed.state else {
        let _ = req.respond(html_response(
            400,
            "<h1>Missing state</h1><p>This callback is invalid.</p>",
        ));
        return CallbackOutcome::KeepListening;
    };

    // Constant-time compare to avoid a `==` length-leak if a future state
    // encoding becomes variable-length.
    if !ct_eq(&got, expected_state) {
        let _ = req.respond(html_response(
            400,
            "<h1>State mismatch</h1><p>Possible CSRF — request rejected.</p>",
        ));
        return CallbackOutcome::Failed(
            "State mismatch in callback — refusing to save token (possible CSRF).".into(),
        );
    }

    let token = match parsed.token {
        Some(t) if !t.is_empty() => t,
        _ => {
            let _ = req.respond(html_response(
                400,
                "<h1>Missing token</h1><p>This callback is invalid.</p>",
            ));
            return CallbackOutcome::KeepListening;
        }
    };

    let _ = req.respond(html_response(
        200,
        "<h1>Logged in</h1><p>You can close this tab and return to your terminal.</p>",
    ));
    CallbackOutcome::Token(BrowserLoginResult {
        token,
        refresh_token: parsed.refresh_token,
    })
}

/// Test-only callback loop over a pre-bound `Server` and known `state`,
/// exposing the worker thread's receiver so tests can drive a synthetic
/// browser request without a real browser.
///
/// Drives the same [`handle_callback_request`] code path as the production
/// flow, then flattens the [`BrowserLoginResult`] down to its token so the
/// test harness can assert on a plain `Result<String, String>`.
#[cfg(test)]
fn run_callback_loop(
    server: Server,
    expected_state: String,
) -> mpsc::Receiver<Result<String, String>> {
    let (tx, rx) = mpsc::channel::<Result<String, String>>();
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(FLOW_TIMEOUT_SECS);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let _ = tx.send(Err("Login timed out after 120s.".into()));
                return;
            }
            let req = match server.recv_timeout(remaining) {
                Ok(Some(r)) => r,
                Ok(None) => {
                    let _ = tx.send(Err("Login timed out.".into()));
                    return;
                }
                Err(e) => {
                    let _ = tx.send(Err(format!("Local server error: {e}")));
                    return;
                }
            };
            match handle_callback_request(req, &expected_state) {
                CallbackOutcome::KeepListening => {}
                CallbackOutcome::Token(result) => {
                    let _ = tx.send(Ok(result.token));
                    return;
                }
                CallbackOutcome::Failed(msg) => {
                    let _ = tx.send(Err(msg));
                    return;
                }
            }
        }
    });
    rx
}

fn web_origin(api_base: &str) -> String {
    difflore_core::cloud::endpoints::web_origin_from(api_base)
}

fn random_state() -> String {
    use std::fmt::Write as _;
    let mut bytes = [0u8; 32];
    // `rand::rng()` returns a thread-local CSPRNG seeded from the OS — fine
    // for a one-shot auth nonce.
    rand::rng().fill(&mut bytes[..]);
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

pub(crate) fn run_browser_login_with_cancel(
    api_base: &str,
    cancel: &Arc<AtomicBool>,
) -> Result<BrowserLoginResult, String> {
    let origin = web_origin(api_base);
    let state = random_state();

    // Bind loopback (not 0.0.0.0) to avoid cross-machine exposure; `:0` lets
    // the OS pick a free port instead of squatting a hard-coded one.
    let server = Server::http("127.0.0.1:0")
        .map_err(|e| format!("Failed to start localhost callback server: {e}"))?;

    let local_addr = server
        .server_addr()
        .to_ip()
        .ok_or_else(|| "Local server did not bind to an IP address".to_owned())?;
    let port = local_addr.port();

    let redirect_uri = format!("http://127.0.0.1:{port}/callback");
    let auth_url = build_auth_url(api_base, &redirect_uri, &state);

    println!("Opening browser to {origin}/cli-auth ...");
    println!("If it doesn't open, paste this URL into your browser:");
    println!("  {auth_url}");
    println!("Waiting for authorization (timeout: 120s)...");

    if let Err(e) = webbrowser::open(&auth_url) {
        eprintln!("warning: could not auto-open browser ({e}). Open the URL above manually.");
    }

    // Worker thread runs the request loop so the main thread can enforce the
    // wall-clock timeout via mpsc::recv_timeout.
    let (tx, rx) = mpsc::channel::<Result<BrowserLoginResult, String>>();
    let expected_state = state;
    let server_cancel = Arc::clone(cancel);

    std::thread::spawn(move || {
        // Loop until one /callback hit with matching state. Anything else
        // (favicon probes, port scanners) gets a 404 and we keep listening.
        let deadline = Instant::now() + Duration::from_secs(FLOW_TIMEOUT_SECS);
        loop {
            if server_cancel.load(Ordering::Relaxed) {
                let _ = tx.send(Err("Login cancelled.".into()));
                return;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let _ = tx.send(Err("Login timed out after 120s.".into()));
                return;
            }

            let wait = remaining.min(Duration::from_millis(250));
            let req = match server.recv_timeout(wait) {
                Ok(Some(r)) => r,
                Ok(None) => {
                    continue;
                }
                Err(e) => {
                    let _ = tx.send(Err(format!("Local server error: {e}")));
                    return;
                }
            };

            match handle_callback_request(req, &expected_state) {
                CallbackOutcome::KeepListening => {}
                CallbackOutcome::Token(result) => {
                    // Linger so the browser receives the body before we tear the
                    // listener down; otherwise fast browsers see ECONNRESET and
                    // render a "site can't be reached" page despite the 200.
                    std::thread::sleep(Duration::from_millis(POST_SUCCESS_LINGER_MS));
                    let _ = tx.send(Ok(result));
                    return;
                }
                CallbackOutcome::Failed(msg) => {
                    let _ = tx.send(Err(msg));
                    return;
                }
            }
        }
    });

    let deadline = Instant::now() + Duration::from_secs(FLOW_TIMEOUT_SECS + 5);
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("Login cancelled.".to_owned());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("Login timed out.".to_owned());
        }
        match rx.recv_timeout(remaining.min(Duration::from_millis(250))) {
            Ok(result) => return result,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("Login worker stopped before returning a token.".to_owned());
            }
        }
    }
}

fn build_auth_url(api_base: &str, redirect_uri: &str, state: &str) -> String {
    let auth_route = difflore_core::cloud::endpoints::web_link_from(api_base, "cli-auth");
    format!(
        "{auth_route}?redirect_uri={r}&state={s}",
        r = urlencode(redirect_uri),
        s = urlencode(state),
    )
}

fn html_response(status: u16, body: &str) -> Response<Cursor<Vec<u8>>> {
    let html = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>DiffLore CLI</title>\
         <style>body{{font-family:system-ui,sans-serif;padding:2rem;max-width:32rem;margin:auto;color:#222;}}h1{{font-size:1.4rem;}}code{{background:#f4f4f4;padding:0.1em 0.3em;border-radius:3px;}}</style>\
         </head><body>{body}</body></html>"
    );
    let bytes = html.into_bytes();
    Response::from_data(bytes)
        .with_status_code(status)
        .with_header(
            // reason: hardcoded ASCII content-type header cannot fail to parse
            #[allow(clippy::expect_used)]
            Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..])
                .expect("hardcoded ASCII content-type header cannot fail to parse"),
        )
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(h), Some(l)) = (hi, lo) {
                    out.push((h * 16 + l) as u8);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn htmlescape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Constant-time string equality, used defensively for the state nonce.
fn ct_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.bytes().zip(b.bytes()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
// reason: test invariants — fail loudly when scaffolding (port bind, etc.) breaks
mod tests {
    use super::*;

    #[test]
    fn web_origin_strips_api_suffix() {
        assert_eq!(
            web_origin("https://difflore.dev/api"),
            "https://difflore.dev"
        );
        assert_eq!(
            web_origin("https://difflore.dev/api/"),
            "https://difflore.dev"
        );
        assert_eq!(
            web_origin("http://localhost:3017/api"),
            "http://localhost:3017"
        );
        // No /api suffix → leave unchanged
        assert_eq!(web_origin("http://localhost:3017"), "http://localhost:3017");
    }

    #[test]
    fn auth_url_handles_api_base_with_path_and_encodes_query() {
        let url = build_auth_url(
            "http://localhost:3017/api/sub/path",
            "http://127.0.0.1:49152/callback?x=1&space=a b",
            "state/with+reserved",
        );

        assert_eq!(
            url,
            "http://localhost:3017/api/sub/path/cli-auth?redirect_uri=http%3A%2F%2F127.0.0.1%3A49152%2Fcallback%3Fx%3D1%26space%3Da%20b&state=state%2Fwith%2Breserved"
        );
    }

    #[test]
    fn random_state_is_64_hex_chars() {
        let s = random_state();
        assert_eq!(s.len(), 64);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn ct_eq_handles_match_and_mismatch() {
        assert!(ct_eq("abc", "abc"));
        assert!(!ct_eq("abc", "abd"));
        assert!(!ct_eq("abc", "ab"));
    }

    fn drive_callback(query: &str, expected_state: &str) -> Result<String, String> {
        let server = Server::http("127.0.0.1:0").expect("bind");
        let port = server.server_addr().to_ip().unwrap().port();
        let rx = run_callback_loop(server, expected_state.to_owned());

        let query = query.to_owned();
        // Synthetic browser hit. We don't actually care about the body —
        // tiny_http just needs a valid HTTP/1.1 request line.
        std::thread::spawn(move || {
            use std::io::Write;
            use std::net::TcpStream;
            let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
            let req = format!(
                "GET /callback?{query} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
            );
            let _ = s.write_all(req.as_bytes());
        });

        rx.recv_timeout(Duration::from_secs(5))
            .map_err(|_| "no result".to_owned())
            .and_then(|r| r)
    }

    #[test]
    fn callback_success_path() {
        let res = drive_callback("token=tok-abc&state=hello", "hello").unwrap();
        assert_eq!(res, "tok-abc");
    }

    /// Drive a synthetic POST `/callback` whose token lives in the
    /// form-urlencoded body (not the request line) — the preferred path that
    /// keeps the secret out of loopback request-line logs.
    fn drive_callback_post(body: &str, expected_state: &str) -> Result<String, String> {
        let server = Server::http("127.0.0.1:0").expect("bind");
        let port = server.server_addr().to_ip().unwrap().port();
        let rx = run_callback_loop(server, expected_state.to_owned());

        let body = body.to_owned();
        std::thread::spawn(move || {
            use std::io::Write;
            use std::net::TcpStream;
            let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
            let req = format!(
                "POST /callback HTTP/1.1\r\nHost: 127.0.0.1\r\n\
                 Content-Type: application/x-www-form-urlencoded\r\n\
                 Content-Length: {len}\r\nConnection: close\r\n\r\n{body}",
                len = body.len(),
            );
            let _ = s.write_all(req.as_bytes());
        });

        rx.recv_timeout(Duration::from_secs(5))
            .map_err(|_| "no result".to_owned())
            .and_then(|r| r)
    }

    #[test]
    fn callback_success_via_post_body() {
        let res = drive_callback_post("token=tok-post&state=hello", "hello").unwrap();
        assert_eq!(res, "tok-post");
    }

    #[test]
    fn callback_post_body_state_is_validated() {
        let res = drive_callback_post("token=tok-post&state=WRONG", "hello");
        assert!(res.is_err(), "got {res:?}");
        assert!(res.unwrap_err().contains("State mismatch"));
    }

    #[test]
    fn callback_query_reads_refresh_token() {
        let parsed = parse_callback_query("token=tok&refreshToken=ref%2Btok&state=hello");

        assert_eq!(parsed.token.as_deref(), Some("tok"));
        assert_eq!(parsed.refresh_token.as_deref(), Some("ref+tok"));
        assert_eq!(parsed.state.as_deref(), Some("hello"));
    }

    #[test]
    fn callback_state_mismatch_returns_err_and_no_token() {
        let res = drive_callback("token=tok-abc&state=WRONG", "hello");
        assert!(res.is_err(), "got {res:?}");
        let msg = res.unwrap_err();
        assert!(msg.contains("State mismatch"), "msg: {msg}");
    }
}
