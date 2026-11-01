//! Userspace TCP port forwarders and the host-routing reverse proxy.

use anyhow::Result;
use std::net::SocketAddrV4;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

/// Forward connections accepted on `listener` -> `target` until `shutdown` flips to true.
pub async fn port_forwarder(
    listener: TcpListener,
    target: SocketAddrV4,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    loop {
        tokio::select! {
            _ = shutdown.changed() => return Ok(()),
            accepted = listener.accept() => {
                let Ok((mut inbound, _)) = accepted else { continue };
                tokio::spawn(async move {
                    match TcpStream::connect(target).await {
                        Ok(mut outbound) => {
                            let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                        }
                        Err(_) => {
                            // Service not listening (yet); just drop the connection.
                        }
                    }
                });
            }
        }
    }
}

/// Minimal HTTP/1.x reverse proxy: routes each connection by its first request's
/// Host header, then splices bytes (so keep-alive and WebSockets work, as long as a
/// connection stays on one host — which browsers guarantee).
pub async fn reverse_proxy(
    listener: TcpListener,
    routes: Vec<(String, SocketAddrV4)>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let routes = Arc::new(routes);
    loop {
        tokio::select! {
            _ = shutdown.changed() => return Ok(()),
            accepted = listener.accept() => {
                let Ok((inbound, _)) = accepted else { continue };
                let routes = routes.clone();
                tokio::spawn(async move { let _ = proxy_conn(inbound, &routes).await; });
            }
        }
    }
}

const MAX_HEADER: usize = 64 * 1024;

async fn proxy_conn(mut inbound: TcpStream, routes: &[(String, SocketAddrV4)]) -> Result<()> {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    while find_header_end(&buf).is_none() {
        let n = inbound.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > MAX_HEADER {
            return respond(&mut inbound, "431 Request Header Fields Too Large", "").await;
        }
    }
    let host = host_header(&buf).unwrap_or_default();
    let Some(target) = route(routes, &host) else {
        let body = format!(
            "ncompose proxy: no route for host {host:?}\nknown hosts: {}\n",
            routes.iter().map(|(h, _)| h.as_str()).collect::<Vec<_>>().join(", ")
        );
        return respond(&mut inbound, "404 Not Found", &body).await;
    };
    let mut outbound = match TcpStream::connect(target).await {
        Ok(s) => s,
        Err(e) => {
            let body = format!("ncompose proxy: {host} -> {target}: {e}\n");
            return respond(&mut inbound, "502 Bad Gateway", &body).await;
        }
    };
    outbound.write_all(&buf).await?;
    tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await?;
    Ok(())
}

fn route(routes: &[(String, SocketAddrV4)], host: &str) -> Option<SocketAddrV4> {
    let host = host.to_ascii_lowercase();
    // Exact match first, then wildcard entries like "*.app.localhost".
    routes.iter().find(|(h, _)| *h == host).map(|(_, t)| *t).or_else(|| {
        routes
            .iter()
            .find(|(h, _)| h.strip_prefix("*.").is_some_and(|suffix| host.ends_with(&format!(".{suffix}"))))
            .map(|(_, t)| *t)
    })
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn host_header(buf: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(&buf[..find_header_end(buf)?]);
    text.lines().skip(1).find_map(|l| {
        let (k, v) = l.split_once(':')?;
        if !k.trim().eq_ignore_ascii_case("host") {
            return None;
        }
        let v = v.trim();
        // Strip the port. IPv6 literals look like "[::1]:80".
        let host = match v.strip_prefix('[') {
            Some(rest) => rest.split(']').next().unwrap_or(rest),
            None => v.split(':').next().unwrap_or(v),
        };
        Some(host.to_string())
    })
}

async fn respond(s: &mut TcpStream, status: &str, body: &str) -> Result<()> {
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(resp.as_bytes()).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_parsing_and_routing() {
        let req = b"GET / HTTP/1.1\r\nHOST: App.Localhost:80\r\nAccept: */*\r\n\r\n";
        assert_eq!(host_header(req).as_deref(), Some("App.Localhost"));
        let routes = vec![
            ("app.localhost".to_string(), "127.10.0.2:8080".parse().unwrap()),
            ("*.dev.localhost".to_string(), "127.10.0.3:80".parse().unwrap()),
        ];
        assert_eq!(route(&routes, "App.Localhost").unwrap().port(), 8080);
        assert_eq!(route(&routes, "x.dev.localhost").unwrap().port(), 80);
        assert!(route(&routes, "other.localhost").is_none());
    }

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn host_header_edge_cases() {
        assert_eq!(host_header(b"GET / HTTP/1.1\r\nHost: [::1]:8080\r\n\r\n").as_deref(), Some("::1"));
        assert_eq!(host_header(b"GET / HTTP/1.1\r\nhost:app.localhost\r\n\r\n").as_deref(), Some("app.localhost"));
        assert_eq!(host_header(b"GET / HTTP/1.1\r\nAccept: */*\r\n\r\n"), None);
        assert_eq!(host_header(b"GET / HTTP/1.1\r\nHost: a\r\n"), None, "headers not complete yet");
        // A "Host:" in the request line is not a header.
        assert_eq!(host_header(b"GET /Host: evil HTTP/1.1\r\nHost: good\r\n\r\n").as_deref(), Some("good"));
    }

    #[test]
    fn exact_routes_win_over_wildcards() {
        let routes = vec![
            ("*.localhost".to_string(), "127.0.0.1:1".parse().unwrap()),
            ("app.localhost".to_string(), "127.0.0.1:2".parse().unwrap()),
        ];
        assert_eq!(route(&routes, "app.localhost").unwrap().port(), 2);
        assert_eq!(route(&routes, "x.localhost").unwrap().port(), 1);
        assert!(route(&routes, "localhost").is_none(), "a wildcard needs a label in front");
    }

    async fn local_listener() -> (TcpListener, SocketAddrV4) {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = match l.local_addr().unwrap() {
            std::net::SocketAddr::V4(a) => a,
            _ => unreachable!(),
        };
        (l, addr)
    }

    /// A backend that answers one HTTP response containing what it was sent.
    async fn http_backend(name: &'static str) -> SocketAddrV4 {
        let (l, addr) = local_listener().await;
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 1024];
                    while find_header_end(&buf).is_none() {
                        let n = s.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    let body = format!("{name} got: {}", String::from_utf8_lossy(&buf));
                    let resp = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    let _ = s.write_all(resp.as_bytes()).await;
                });
            }
        });
        addr
    }

    async fn request(addr: SocketAddrV4, parts: &[&[u8]]) -> String {
        let mut s = TcpStream::connect(addr).await.unwrap();
        for p in parts {
            s.write_all(p).await.unwrap();
            s.flush().await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let mut out = String::new();
        tokio::time::timeout(std::time::Duration::from_secs(5), s.read_to_string(&mut out))
            .await
            .unwrap()
            .unwrap();
        out
    }

    #[tokio::test]
    async fn port_forwarder_relays_both_ways_until_shutdown() {
        let (backend, backend_addr) = local_listener().await;
        tokio::spawn(async move {
            while let Ok((mut s, _)) = backend.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 64];
                    while let Ok(n) = s.read(&mut buf).await {
                        if n == 0 || s.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        let (front, front_addr) = local_listener().await;
        let (stop, stopped) = watch::channel(false);
        let task = tokio::spawn(port_forwarder(front, backend_addr, stopped));

        let mut c = TcpStream::connect(front_addr).await.unwrap();
        c.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        c.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");

        stop.send(true).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), task).await.unwrap().unwrap().unwrap();
    }

    #[tokio::test]
    async fn reverse_proxy_routes_by_host_and_explains_failures() {
        let app = http_backend("app").await;
        let (dead_listener, dead) = local_listener().await;
        drop(dead_listener); // nothing listens there any more
        let routes = vec![("app.localhost".to_string(), app), ("dead.localhost".to_string(), dead)];
        let (front, front_addr) = local_listener().await;
        let (_stop, stopped) = watch::channel(false);
        tokio::spawn(reverse_proxy(front, routes, stopped));

        // Headers arriving in pieces are buffered and forwarded whole.
        let resp = request(front_addr, &[b"GET /x HTTP/1.1\r\nHo", b"st: App.Localhost:8088\r\n\r\n"]).await;
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        assert!(resp.contains("app got: GET /x HTTP/1.1\r\nHost: App.Localhost:8088\r\n\r\n"), "{resp}");

        let resp = request(front_addr, &[b"GET / HTTP/1.1\r\nHost: nope.localhost\r\n\r\n"]).await;
        assert!(resp.starts_with("HTTP/1.1 404"), "{resp}");
        assert!(resp.contains("app.localhost, dead.localhost"), "{resp}");

        let resp = request(front_addr, &[b"GET / HTTP/1.1\r\nHost: dead.localhost\r\n\r\n"]).await;
        assert!(resp.starts_with("HTTP/1.1 502"), "{resp}");
    }

    #[tokio::test]
    async fn reverse_proxy_refuses_oversized_headers() {
        let (front, front_addr) = local_listener().await;
        let (_stop, stopped) = watch::channel(false);
        tokio::spawn(reverse_proxy(front, vec![], stopped));
        let mut s = TcpStream::connect(front_addr).await.unwrap();
        let junk = vec![b'a'; MAX_HEADER + 10];
        let _ = s.write_all(b"GET / HTTP/1.1\r\nX: ").await;
        let _ = s.write_all(&junk).await;
        let mut out = String::new();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), s.read_to_string(&mut out)).await;
        assert!(out.starts_with("HTTP/1.1 431"), "{out}");
    }
}
