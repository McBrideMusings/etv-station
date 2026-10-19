//! The one HTTP call a Rhai script can make, and `parse_json` to read what it
//! returns (ADR 0014). Event hooks reach it as a granted `http(url, method,
//! body)` ([`crate::hooks`]); pool and bumper scripts reach it as
//! `ctx.http(url, method, body)` once they declare the `http` capability and
//! their channel grants it ([`crate::score`], [`crate::bumper::hook`]).
//!
//! Every call has a timeout, so a hung server cannot hold a generation or a
//! hook worker indefinitely. Errors and logs carry the method and host, never
//! the full URL: a URL may carry a token in its query.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rhai::{Dynamic, EvalAltResult};

/// Longest a `ctx.http` call made during generation waits for a response.
pub const GENERATION_TIMEOUT: Duration = Duration::from_secs(10);

/// Most time one script call (`pick()`, `build()`) may spend in outside calls
/// — `ctx.http` and `ctx.exec` together — however many it makes.
pub const GENERATION_BUDGET: Duration = Duration::from_secs(30);

/// The time one script call has left for outside calls. Clones share the
/// remainder, so the `ctx` a script holds and the engine's copy draw from one
/// budget.
#[derive(Debug, Clone)]
pub struct Budget {
    total: Duration,
    left: Arc<Mutex<Duration>>,
}

impl Budget {
    pub fn new(total: Duration) -> Self {
        Self {
            total,
            left: Arc::new(Mutex::new(total)),
        }
    }

    /// Runs `f` with a timeout of [`GENERATION_TIMEOUT`] or what is left,
    /// whichever is shorter, and charges the time it took. With nothing left
    /// it does not run `f`: the error names the budget and the log line
    /// carries it.
    pub fn within<T>(
        &self,
        what: &str,
        f: impl FnOnce(Duration) -> Result<T, String>,
    ) -> Result<T, String> {
        let left = *self.left.lock().unwrap_or_else(|e| e.into_inner());
        if left.is_zero() {
            tracing::warn!(
                event = "plugin.http",
                call = what,
                budget_s = self.total.as_secs_f64(),
                "plugin script spent its budget for outside calls"
            );
            return Err(format!(
                "{what}: this script call's {}s budget for outside calls (ctx.http, ctx.exec) is spent",
                self.total.as_secs()
            ));
        }
        let started = Instant::now();
        let result = f(left.min(GENERATION_TIMEOUT)).map_err(|e| format!("{what}: {e}"));
        let mut left = self.left.lock().unwrap_or_else(|e| e.into_inner());
        *left = left.saturating_sub(started.elapsed());
        result
    }
}

/// The environment variables a plugin may name as `{{NAME}}` in a
/// [`call`]'s URL: where the station reaches Plex. The value is substituted
/// here, in the station, so a script can reach Plex without ever holding the
/// token; the response is the script's to read.
pub const URL_ENV: &[&str] = &["PLEX_URL", "PLEX_TOKEN"];

/// `url` with each `{{NAME}}` for a [`URL_ENV`] name replaced by that
/// variable's value. Any other `{{...}}` is left as written. A named variable
/// that is unset or empty is an error naming it, never a request to a
/// half-built URL.
fn expand_url(url: &str) -> Result<String, String> {
    expand_url_with(url, |name| std::env::var(name).ok())
}

fn expand_url_with(url: &str, lookup: impl Fn(&str) -> Option<String>) -> Result<String, String> {
    let mut out = url.to_string();
    for name in URL_ENV {
        let token = format!("{{{{{name}}}}}");
        if !out.contains(&token) {
            continue;
        }
        let value = lookup(name)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| format!("{name} is not set, but the url names {token}"))?;
        out = out.replace(&token, &value);
    }
    // The token goes only to the host the station itself reaches Plex at, so a
    // script cannot send it elsewhere by writing a different host in the url.
    let token = "{{PLEX_TOKEN}}";
    if url.contains(token) {
        let plex = lookup("PLEX_URL").unwrap_or_default();
        if host_of(&out) != host_of(&plex) {
            return Err(format!(
                "the url names {token} but its host is not the PLEX_URL host"
            ));
        }
    }
    Ok(out)
}

/// The host `url` reaches, for a log line: `{{PLEX_URL}}` is filled in (left
/// as written when unset), and the credentials, path and query are dropped.
pub fn display_host(url: &str) -> String {
    let plex = std::env::var("PLEX_URL").unwrap_or_default();
    let shown = if plex.is_empty() {
        url.to_string()
    } else {
        url.replace("{{PLEX_URL}}", &plex)
    };
    host_of(&shown).to_string()
}

/// `method` `url` with `body` (sent only when non-empty), returning the
/// response body as text. A 4xx/5xx status, a transport failure and a timeout
/// are all errors. `{{PLEX_URL}}` and `{{PLEX_TOKEN}}` in `url` are filled from
/// the environment ([`URL_ENV`]).
pub fn call(url: &str, method: &str, body: &str, timeout: Duration) -> Result<String, String> {
    call_with(url, method, body, &[], timeout)
}

/// [`call`] with request headers, each a `(name, value)` pair (for example
/// `Accept: application/json`, which Plex needs to answer in JSON).
pub fn call_with(
    url: &str,
    method: &str,
    body: &str,
    headers: &[(String, String)],
    timeout: Duration,
) -> Result<String, String> {
    let url = expand_url(url)?;
    let url = url.as_str();
    let method = method.to_uppercase();
    let host = host_of(url);
    let started = Instant::now();
    let agent = ureq::AgentBuilder::new().timeout(timeout).build();
    let req = headers
        .iter()
        .fold(agent.request(&method, url), |req, (k, v)| req.set(k, v));
    let resp = if body.is_empty() {
        req.call()
    } else {
        req.send_string(body)
    };
    let elapsed_ms = started.elapsed().as_millis() as u64;
    match resp {
        Ok(r) => {
            let status = r.status();
            // After a redirect this is the host that actually answered.
            let answered = host_of(r.get_url()).to_string();
            let text = r.into_string().map_err(|e| {
                tracing::warn!(
                    event = "plugin.http",
                    method = %method,
                    host = %answered,
                    status,
                    error = %e,
                    elapsed_ms,
                    "plugin http call failed reading the body"
                );
                format!("http {method} {answered}: reading the body failed: {e}")
            })?;
            tracing::info!(
                event = "plugin.http",
                method = %method,
                host = %answered,
                status,
                bytes = text.len(),
                elapsed_ms,
                "plugin script made an http call"
            );
            Ok(text)
        }
        Err(ureq::Error::Status(code, r)) => {
            let answered = host_of(r.get_url());
            tracing::warn!(
                event = "plugin.http",
                method = %method,
                host = answered,
                status = code,
                elapsed_ms,
                "plugin http call returned an error status"
            );
            Err(format!("http {method} {answered} -> status {code}"))
        }
        // ureq puts the URL in its error's Display; the kind alone does not.
        Err(ureq::Error::Transport(t)) => {
            let kind = t.kind();
            tracing::warn!(
                event = "plugin.http",
                method = %method,
                host,
                error = %kind,
                elapsed_ms,
                "plugin http call failed"
            );
            Err(format!("http {method} {host} failed: {kind}"))
        }
    }
}

/// `parse_json(text)`: JSON text as Rhai values — objects become maps, arrays
/// arrays, `null` becomes `()`.
pub fn parse_json(text: &str) -> Result<Dynamic, Box<EvalAltResult>> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("parse_json(): {e}"))?;
    rhai::serde::to_dynamic(value)
}

/// The host part of `url`, without scheme, credentials, path or query.
pub fn host_of(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    authority.rsplit_once('@').map_or(authority, |(_, h)| h)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// A server on a free local port that answers one request with `status`
    /// and `body`, and hands back the request line it received.
    pub(crate) fn serve_once(
        status: &'static str,
        body: &'static str,
    ) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            // Read the whole request, body included: closing with unread bytes
            // makes the OS reset the connection, and the client sees that as
            // "Network Error" instead of the status.
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            let (head_len, body_len) = loop {
                let n = sock.read(&mut buf).unwrap();
                raw.extend_from_slice(&buf[..n]);
                if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&raw[..pos]).to_ascii_lowercase();
                    let body_len = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    break (pos + 4, body_len);
                }
                assert!(n > 0, "client closed before finishing its headers");
            };
            while raw.len() < head_len + body_len {
                let n = sock.read(&mut buf).unwrap();
                assert!(n > 0, "client closed before finishing its body");
                raw.extend_from_slice(&buf[..n]);
            }
            let request = String::from_utf8_lossy(&raw).into_owned();
            let reply = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(reply.as_bytes()).unwrap();
            // Close the way TCP intends: send FIN, then wait for the client to
            // close its end. Dropping the socket outright lets the OS reset the
            // connection if anything is still in flight, and a reset can
            // overtake the response.
            let _ = sock.shutdown(std::net::Shutdown::Write);
            let _ = sock.set_read_timeout(Some(Duration::from_secs(5)));
            while matches!(sock.read(&mut buf), Ok(n) if n > 0) {}
            request.lines().next().unwrap_or_default().to_string()
        });
        (url, handle)
    }

    #[test]
    fn a_server_that_never_answers_times_out() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/slow?token=secret",
            listener.local_addr().unwrap()
        );
        let started = Instant::now();
        let e = call(&url, "get", "", Duration::from_millis(300)).unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(5), "{e}");
        assert!(!e.contains("secret"), "error leaked the query: {e}");
        drop(listener);
    }

    #[test]
    fn fifty_calls_to_a_server_that_never_answers_stop_at_the_budget() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let budget = Budget::new(Duration::from_millis(600));
        let started = Instant::now();
        let mut last = Ok(String::new());
        for _ in 0..50 {
            last = budget.within("ctx.http()", |t| call(&url, "get", "", t));
            if last.is_err() && started.elapsed() > Duration::from_millis(600) {
                break;
            }
        }
        let calls_done = started.elapsed();
        assert!(calls_done < Duration::from_secs(3), "{calls_done:?}");
        let e = last.unwrap_err();
        assert!(e.contains("http"), "{e}");
        let e = budget
            .within("ctx.http()", |t| call(&url, "get", "", t))
            .unwrap_err();
        assert!(e.contains("budget") && e.contains("is spent"), "{e}");
        drop(listener);
    }

    #[test]
    fn an_error_status_is_an_error_naming_the_host() {
        let (url, server) = serve_once("503 Service Unavailable", "");
        let e = call(
            &format!("{url}/x?token=secret"),
            "post",
            "{}",
            GENERATION_TIMEOUT,
        )
        .unwrap_err();
        assert_eq!(server.join().unwrap(), "POST /x?token=secret HTTP/1.1");
        assert!(e.ends_with("-> status 503"), "{e}");
        assert!(!e.contains("secret"), "{e}");
    }

    #[test]
    fn parse_json_reads_objects_arrays_and_null() {
        let v = parse_json(r#"{"a": [1, "two", null], "b": {"c": true}}"#).unwrap();
        let m = v.cast::<rhai::Map>();
        let a = m["a"].clone().cast::<rhai::Array>();
        assert_eq!(a[0].as_int().unwrap(), 1);
        assert_eq!(a[1].clone().into_string().unwrap(), "two");
        assert!(a[2].is_unit());
        let b = m["b"].clone().cast::<rhai::Map>();
        assert!(b["c"].as_bool().unwrap());
        assert!(
            parse_json("{nope")
                .unwrap_err()
                .to_string()
                .contains("parse_json()")
        );
    }

    #[test]
    fn a_url_names_plex_by_placeholder_and_nothing_else() {
        let env = |name: &str| match name {
            "PLEX_URL" => Some("http://plex.lan:32400".to_string()),
            "PLEX_TOKEN" => Some("tok".to_string()),
            _ => None,
        };
        let got = expand_url_with(
            "{{PLEX_URL}}/x?X-Plex-Token={{PLEX_TOKEN}}&o={{OTHER}}",
            env,
        );
        assert_eq!(
            got.unwrap(),
            "http://plex.lan:32400/x?X-Plex-Token=tok&o={{OTHER}}"
        );
        let elsewhere =
            expand_url_with("https://evil.example/c?t={{PLEX_TOKEN}}", env).unwrap_err();
        assert!(elsewhere.contains("not the PLEX_URL host"), "{elsewhere}");
        let e = expand_url_with("{{PLEX_URL}}/x", |_| None).unwrap_err();
        assert!(e.contains("PLEX_URL is not set"), "{e}");
        let empty = expand_url_with("{{PLEX_TOKEN}}", |_| Some(String::new())).unwrap_err();
        assert!(empty.contains("PLEX_TOKEN is not set"), "{empty}");
    }

    #[test]
    fn host_of_drops_credentials_path_and_query() {
        assert_eq!(
            host_of("https://u:p@plex.local:32400/a?t=1"),
            "plex.local:32400"
        );
        assert_eq!(host_of("http://h?x"), "h");
    }
}
