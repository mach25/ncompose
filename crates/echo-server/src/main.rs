//! A native stand-in for the `jmalloc/echo-server` image.
//!
//! - Any HTTP request: `200 text/plain` with a dump of the request
//!   ("Request served by <host>", the request line, headers, body).
//! - A WebSocket upgrade on any path: greets with "Request served by <host>",
//!   then echoes every text and binary frame back.
//! - `LOG_HTTP_HEADERS` / `LOG_HTTP_BODY`: logs each request's line and headers,
//!   and its body as Go `hex.Dump` rows (`00000000  6c 6f ...  |lo...|`), the
//!   format readers of the image's log already parse.
//!
//! Listens on `0.0.0.0:$PORT` (default 8080).

use futures_util::{SinkExt, StreamExt};
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::header::{HeaderValue, CONNECTION, CONTENT_TYPE, UPGRADE};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::convert::Infallible;
use std::fmt::Write as _;
use std::io::Write as _;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

struct Config {
    host: String,
    log_headers: bool,
    log_body: bool,
}

fn flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false"))
}

#[tokio::main]
async fn main() {
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8080);
    let config = Arc::new(Config {
        host: std::env::var("HOSTNAME").unwrap_or_else(|_| "echo-server".into()),
        log_headers: flag("LOG_HTTP_HEADERS"),
        log_body: flag("LOG_HTTP_BODY"),
    });
    let listener = match TcpListener::bind(("0.0.0.0", port)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("echo-server: cannot listen on port {port}: {e}");
            std::process::exit(1);
        }
    };
    println!("Echo server listening on port {port}.");
    loop {
        let Ok((stream, remote)) = listener.accept().await else { continue };
        let config = config.clone();
        tokio::spawn(async move {
            let service = service_fn(move |req| handle(req, remote, config.clone()));
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .with_upgrades()
                .await;
        });
    }
}

async fn handle(
    req: Request<Incoming>,
    remote: SocketAddr,
    config: Arc<Config>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    if is_websocket_upgrade(&req) {
        return Ok(websocket(req, remote, config));
    }
    let (parts, body) = req.into_parts();
    let body = body.collect().await.map(|b| b.to_bytes()).unwrap_or_default();

    let mut head = format!("{} {} {:?}\n", parts.method, parts.uri, parts.version);
    let mut headers: Vec<(String, String)> = parts
        .headers
        .iter()
        .map(|(k, v)| (canonical(k.as_str()), String::from_utf8_lossy(v.as_bytes()).into_owned()))
        .collect();
    headers.sort();
    for (k, v) in &headers {
        let _ = writeln!(head, "{k}: {v}");
    }

    log_request(&config, remote, &head, &body);

    let mut text = format!("Request served by {}\n\n", config.host);
    text.push_str(&head);
    text.push('\n');
    text.push_str(&String::from_utf8_lossy(&body));
    let mut resp = Response::new(Full::new(Bytes::from(text)));
    resp.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    Ok(resp)
}

/// One write per request, so concurrent requests never interleave in the log.
fn log_request(config: &Config, remote: SocketAddr, head: &str, body: &[u8]) {
    if !config.log_headers && !config.log_body {
        return;
    }
    let mut out = String::new();
    if config.log_headers {
        let mut lines = head.lines();
        if let Some(request_line) = lines.next() {
            let _ = writeln!(out, "{remote} | {request_line}");
        }
        for l in lines {
            let _ = writeln!(out, "{l}");
        }
    }
    if config.log_body && !body.is_empty() {
        out.push_str(&hex_dump(body));
    }
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(out.as_bytes());
    let _ = stdout.flush();
}

/// `Content-Type` style, as Go's net/http prints header names.
fn canonical(name: &str) -> String {
    name.split('-')
        .map(|part| {
            let mut c = part.chars();
            match c.next() {
                Some(f) => f.to_ascii_uppercase().to_string() + &c.as_str().to_ascii_lowercase(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join("-")
}

/// Go's `encoding/hex.Dump` format.
fn hex_dump(data: &[u8]) -> String {
    let mut out = String::new();
    for (row, chunk) in data.chunks(16).enumerate() {
        let _ = write!(out, "{:08x}  ", row * 16);
        for i in 0..16 {
            match chunk.get(i) {
                Some(b) => {
                    let _ = write!(out, "{b:02x} ");
                }
                None => out.push_str("   "),
            }
            if i == 7 {
                out.push(' ');
            }
        }
        out.push_str(" |");
        out.extend(chunk.iter().map(|&b| if (32..=126).contains(&b) { b as char } else { '.' }));
        out.push_str("|\n");
    }
    out
}

fn is_websocket_upgrade(req: &Request<Incoming>) -> bool {
    let has = |name, token: &str| {
        req.headers()
            .get_all(name)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .any(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case(token)))
    };
    has(UPGRADE, "websocket") && has(CONNECTION, "upgrade") && req.headers().contains_key("sec-websocket-key")
}

fn websocket(mut req: Request<Incoming>, remote: SocketAddr, config: Arc<Config>) -> Response<Full<Bytes>> {
    let key = req.headers().get("sec-websocket-key").map(|k| k.as_bytes().to_vec()).unwrap_or_default();
    let accept = derive_accept_key(&key);
    if config.log_headers {
        println!("{remote} | {} {} (websocket upgrade)", req.method(), req.uri());
    }
    let upgrade = hyper::upgrade::on(&mut req);
    let host = config.host.clone();
    tokio::spawn(async move {
        let Ok(upgraded) = upgrade.await else { return };
        let mut ws = WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Server, None).await;
        if ws.send(Message::text(format!("Request served by {host}"))).await.is_err() {
            return;
        }
        while let Some(Ok(msg)) = ws.next().await {
            let reply = match msg {
                Message::Text(_) | Message::Binary(_) => msg,
                Message::Close(_) => break,
                _ => continue, // pings are answered by tungstenite
            };
            if ws.send(reply).await.is_err() {
                break;
            }
        }
    });
    let mut resp = Response::new(Full::new(Bytes::new()));
    *resp.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    let h = resp.headers_mut();
    h.insert(UPGRADE, HeaderValue::from_static("websocket"));
    h.insert(CONNECTION, HeaderValue::from_static("Upgrade"));
    if let Ok(v) = HeaderValue::from_str(&accept) {
        h.insert("sec-websocket-accept", v);
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_dump_matches_go() {
        // Go: hex.Dump([]byte("logout_token=eyJhbGciOi"))
        assert_eq!(
            hex_dump(b"logout_token=eyJhbGciOi"),
            "00000000  6c 6f 67 6f 75 74 5f 74  6f 6b 65 6e 3d 65 79 4a  |logout_token=eyJ|\n\
             00000010  68 62 47 63 69 4f 69                              |hbGciOi|\n"
        );
    }

    #[test]
    fn canonical_header_names() {
        assert_eq!(canonical("content-type"), "Content-Type");
        assert_eq!(canonical("x-forwarded-for"), "X-Forwarded-For");
    }
}
