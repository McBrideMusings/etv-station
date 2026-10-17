//! The one HTTP call a Rhai script can make, and `parse_json` to read what it
//! returns (ADR 0014). Event hooks reach it as a granted `http(url, method,
//! body)` ([`crate::hooks`]); pool and bumper scripts reach it as
//! `ctx.http(url, method, body)` once they declare the `http` capability and
//! their channel grants it ([`crate::score`], [`crate::bumper::hook`]).
//!
//! Every call has a timeout, so a hung server cannot hold a generation or a
//! hook worker indefinitely. Errors and logs carry the method and host, never
//! the full URL: a URL may carry a token in its query.

use std::time::{Duration, Instant};

use rhai::{Dynamic, EvalAltResult};

/// Longest a `ctx.http` call made during generation waits for a response.
pub const GENERATION_TIMEOUT: Duration = Duration::from_secs(10);

/// `method` `url` with `body` (sent only when non-empty), returning the
/// response body as text. A 4xx/5xx status, a transport failure and a timeout
/// are all errors.
pub fn call(url: &str, method: &str, body: &str, timeout: Duration) -> Result<String, String> {
    let method = method.to_uppercase();
    let host = host_of(url);
    let started = Instant::now();
    let agent = ureq::AgentBuilder::new().timeout(timeout).build();
    let req = agent.request(&method, url);
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
fn host_of(url: &str) -> &str {
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
            let mut buf = [0u8; 4096];
            let n = sock.read(&mut buf).unwrap();
            let request = String::from_utf8_lossy(&buf[..n]).into_owned();
            let reply = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(reply.as_bytes()).unwrap();
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
    fn host_of_drops_credentials_path_and_query() {
        assert_eq!(
            host_of("https://u:p@plex.local:32400/a?t=1"),
            "plex.local:32400"
        );
        assert_eq!(host_of("http://h?x"), "h");
    }
}
