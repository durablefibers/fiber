//! `http_request`: call a URL, durably, with retries that survive a restart.
//!
//! This is the first task a user can drive entirely from input, which is what makes it the
//! first one with a threat model. It makes **the API process** issue requests, and the API
//! sits on the backend network holding database credentials — so an unrestricted version
//! would let any project writer reach `169.254.169.254`, `localhost`, or anything else the
//! server can see but they cannot. That is a genuine escalation beyond running code on an
//! agent, and the guard below is the reason this task is safe to expose.

use crate::context::FiberContext;
use crate::registry::FiberHandler;
use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::net::IpAddr;
use std::time::Duration;

/// Response body kept in the fiber result. Enough to see what came back, bounded so a
/// large response cannot bloat every row of the `fibers` table.
const MAX_BODY_SNIPPET: usize = 2048;
const DEFAULT_TIMEOUT_SECS: u64 = 30;
const MAX_TIMEOUT_SECS: u64 = 300;
const DEFAULT_RETRIES: u32 = 3;
const MAX_RETRIES: u32 = 10;

/// Address ranges a task must not reach, and why. Returning the reason rather than a bool
/// so the failure tells an operator what rule they hit.
fn blocked_reason(ip: IpAddr) -> Option<&'static str> {
    // An IPv4-mapped IPv6 address is the same host by another spelling; unwrap it first or
    // ::ffff:127.0.0.1 walks straight through the v4 checks below.
    let ip = match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => ip,
        },
        v4 => v4,
    };
    match ip {
        IpAddr::V4(a) => {
            if a.is_loopback() {
                Some("loopback")
            } else if a.is_private() {
                Some("private network")
            } else if a.is_link_local() {
                // 169.254.169.254 is the cloud metadata endpoint on every major provider.
                Some("link-local (cloud metadata lives here)")
            } else if a.is_unspecified() {
                Some("unspecified")
            } else if a.is_broadcast() || a.is_multicast() {
                Some("broadcast or multicast")
            } else if a.octets()[0] == 100 && (64..128).contains(&a.octets()[1]) {
                Some("carrier-grade NAT")
            } else {
                None
            }
        }
        IpAddr::V6(a) => {
            if a.is_loopback() {
                Some("loopback")
            } else if a.is_unspecified() {
                Some("unspecified")
            } else if a.is_multicast() {
                Some("multicast")
            } else if (a.segments()[0] & 0xfe00) == 0xfc00 {
                Some("unique local")
            } else if (a.segments()[0] & 0xffc0) == 0xfe80 {
                Some("link-local")
            } else {
                None
            }
        }
    }
}

/// Whether private addresses are permitted. Off by default: an instance where the task may
/// reach internal services is a deliberate choice by whoever runs it, not by whoever writes
/// a pipeline.
fn allow_private() -> bool {
    matches!(
        std::env::var("FIBER_HTTP_TASK_ALLOW_PRIVATE").as_deref(),
        Ok("1") | Ok("true")
    )
}

struct Request {
    url: reqwest::Url,
    method: reqwest::Method,
    headers: Vec<(String, String)>,
    body: Option<String>,
    timeout: Duration,
    retries: u32,
}

fn parse_input(input: &Value) -> Result<Request> {
    let url = input
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("http_request needs a `url`"))?;
    let url: reqwest::Url = url.parse().map_err(|e| anyhow!("url: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("url scheme must be http or https, got `{}`", url.scheme());
    }

    let method = input
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("POST")
        .to_ascii_uppercase();
    let method = reqwest::Method::from_bytes(method.as_bytes())
        .map_err(|_| anyhow!("unusable method `{method}`"))?;

    let mut headers = Vec::new();
    if let Some(map) = input.get("headers").and_then(Value::as_object) {
        for (k, v) in map {
            let Some(v) = v.as_str() else {
                bail!("header `{k}` must be a string");
            };
            // A header name or value carrying a newline is how one request becomes two.
            if k.is_empty() || k.contains(['\n', '\r', ':']) || v.contains(['\n', '\r']) {
                bail!("header `{k}` has an unusable name or value");
            }
            headers.push((k.clone(), v.to_string()));
        }
    }

    let body = match input.get("body") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(other) => Some(other.to_string()),
    };

    let timeout = Duration::from_secs(
        input
            .get("timeout_seconds")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .clamp(1, MAX_TIMEOUT_SECS),
    );
    let retries = input
        .get("retries")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_RETRIES as u64)
        .min(MAX_RETRIES as u64) as u32;

    Ok(Request {
        url,
        method,
        headers,
        body,
        timeout,
        retries,
    })
}

/// Resolve the host and refuse any address the guard blocks.
///
/// Every resolved address is checked, not just the first: a name that returns one public
/// and one private address must not be usable to reach the private one.
async fn check_destination(url: &reqwest::Url) -> Result<()> {
    if allow_private() {
        return Ok(());
    }
    let host = url.host_str().ok_or_else(|| anyhow!("url has no host"))?;
    let port = url.port_or_known_default().unwrap_or(443);

    // A literal address needs no lookup, and must be checked as given.
    if let Ok(ip) = host.parse::<IpAddr>() {
        if let Some(why) = blocked_reason(ip) {
            bail!("refusing to call {host}: {why} address");
        }
        return Ok(());
    }

    let addrs = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| anyhow!("resolve {host}: {e}"))?;
    let mut any = false;
    for addr in addrs {
        any = true;
        if let Some(why) = blocked_reason(addr.ip()) {
            bail!("refusing to call {host}: resolves to a {why} address");
        }
    }
    if !any {
        bail!("{host} did not resolve");
    }
    Ok(())
}

/// One attempt. Never returns `Err` for an HTTP-level outcome — the caller decides what is
/// worth retrying, and a step that returned `Err` would fail the whole fiber instead.
async fn attempt(req: &Request, idempotency_key: &str) -> Value {
    if let Err(e) = check_destination(&req.url).await {
        // Not retryable: resolution will not become allowed on the next go.
        return json!({ "ok": false, "retryable": false, "error": format!("{e:#}") });
    }
    let client = match reqwest::Client::builder()
        .timeout(req.timeout)
        // A redirect is a second destination the guard never saw. Whoever wants to call
        // the target of a redirect can name it directly.
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return json!({ "ok": false, "retryable": false, "error": format!("client: {e}") });
        }
    };

    let mut rb = client.request(req.method.clone(), req.url.clone());
    for (k, v) in &req.headers {
        rb = rb.header(k, v);
    }
    // The same fiber retrying is the same logical call. A receiver that honours this can
    // make an at-least-once delivery idempotent, which is the only real answer to the
    // duplicate a crash between send and checkpoint can produce.
    rb = rb.header("Idempotency-Key", idempotency_key);
    if let Some(b) = &req.body {
        rb = rb.body(b.clone());
    }

    match rb.send().await {
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            let snippet: String = body.chars().take(MAX_BODY_SNIPPET).collect();
            json!({
                "ok": status.is_success(),
                // 4xx will not change by trying again; 5xx and 429 might.
                "retryable": status.is_server_error() || status.as_u16() == 429,
                "status": status.as_u16(),
                "body": snippet,
            })
        }
        Err(e) => json!({
            "ok": false,
            "retryable": true,
            "error": format!("{e}"),
        }),
    }
}

pub struct HttpRequestTask;

#[async_trait]
impl FiberHandler for HttpRequestTask {
    async fn run(&self, ctx: &mut FiberContext) -> Result<Value> {
        let req = parse_input(&ctx.input)?;
        let fiber_id = ctx.record.id;

        let mut last = Value::Null;
        let mut made = 0u32;
        for n in 0..=req.retries {
            made = n + 1;
            // Memoized: a fiber resumed after a crash does not re-issue an attempt it
            // already made and recorded.
            let key = format!("attempt-{n}");
            let idem = format!("{fiber_id}:{n}");
            let outcome = ctx
                .step(&key, || async { Ok(attempt(&req, &idem).await) })
                .await?;
            last = outcome.clone();

            if outcome.get("ok").and_then(Value::as_bool) == Some(true) {
                return Ok(json!({ "attempts": n + 1, "response": outcome }));
            }
            if outcome.get("retryable").and_then(Value::as_bool) != Some(true) {
                break;
            }
            if n < req.retries {
                // Durable backoff: the wait is a suspension, not a held task, so it
                // survives a restart and costs nothing while it waits.
                let backoff = 2i64.saturating_pow(n).min(300);
                ctx.sleep(backoff).await.map_err(anyhow::Error::new)?;
            }
        }
        // How many were actually made, not how many were allowed: a 404 stops after one,
        // and "after 4 attempts" would send someone looking for three requests that were
        // never sent.
        bail!("http_request failed after {made} attempt(s): {last}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_addresses_that_matter_are_blocked() {
        for (addr, why) in [
            ("127.0.0.1", "loopback"),
            ("10.0.0.5", "private"),
            ("172.16.4.2", "private"),
            ("192.168.1.1", "private"),
            ("169.254.169.254", "link-local"),
            ("100.64.0.1", "carrier"),
            ("0.0.0.0", "unspecified"),
            ("::1", "loopback v6"),
            ("fd00::1", "unique local v6"),
            ("fe80::1", "link-local v6"),
            // The same loopback host spelled as IPv6 must not slip past the v4 checks.
            ("::ffff:127.0.0.1", "mapped loopback"),
            ("::ffff:169.254.169.254", "mapped metadata"),
        ] {
            let ip: IpAddr = addr.parse().unwrap();
            assert!(blocked_reason(ip).is_some(), "{addr} ({why}) was allowed");
        }
    }

    #[test]
    fn ordinary_public_addresses_are_allowed() {
        for addr in [
            "1.1.1.1",
            "93.184.216.34",
            "2606:2800:220:1:248:1893:25c8:1946",
        ] {
            let ip: IpAddr = addr.parse().unwrap();
            assert!(blocked_reason(ip).is_none(), "{addr} was blocked");
        }
    }

    #[test]
    fn input_is_validated_before_anything_is_sent() {
        assert!(parse_input(&json!({})).is_err(), "missing url");
        assert!(
            parse_input(&json!({"url": "file:///etc/passwd"})).is_err(),
            "non-http scheme"
        );
        assert!(
            parse_input(&json!({"url": "https://x/", "headers": {"X-A": "one\r\nX-B: two"}}))
                .is_err(),
            "header value with CRLF should be refused"
        );
        assert!(
            parse_input(&json!({"url": "https://x/", "headers": {"bad\nname": "v"}})).is_err(),
            "header name with a newline should be refused"
        );
        let ok = parse_input(&json!({"url": "https://example.com/hook"})).unwrap();
        assert_eq!(ok.method, reqwest::Method::POST, "default method");
        assert_eq!(ok.retries, DEFAULT_RETRIES);
    }

    #[test]
    fn limits_are_clamped_rather_than_trusted() {
        let r = parse_input(&json!({
            "url": "https://example.com/",
            "timeout_seconds": 100_000,
            "retries": 9_999
        }))
        .unwrap();
        assert_eq!(r.timeout, Duration::from_secs(MAX_TIMEOUT_SECS));
        assert_eq!(r.retries, MAX_RETRIES);
    }

    #[tokio::test]
    async fn a_literal_private_address_is_refused_without_resolving() {
        let url: reqwest::Url = "http://169.254.169.254/latest/meta-data/".parse().unwrap();
        let err = check_destination(&url).await.unwrap_err().to_string();
        assert!(err.contains("link-local"), "{err}");
    }
}
