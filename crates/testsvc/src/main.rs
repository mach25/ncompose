//! Minimal HTTP/1.0 test service.
//!
//! Usage: testsvc [BIND_ADDR]   (default 0.0.0.0:8080, or $PORT)
//!   GET /            -> "hello from <NAME> local=<addr> peer=<addr>"
//!   GET /fetch/<host:port>/<path>  -> fetches http://<host:port>/<path> and returns it
//!                                    (exercises name resolution + connect)
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

fn main() {
    let bind = std::env::args().nth(1).unwrap_or_else(|| {
        format!("0.0.0.0:{}", std::env::var("PORT").unwrap_or_else(|_| "8080".into()))
    });
    let name = std::env::var("NAME").unwrap_or_else(|_| "testsvc".into());
    let listener = TcpListener::bind(&bind).unwrap_or_else(|e| {
        eprintln!("bind {bind}: {e}");
        std::process::exit(1)
    });
    println!("{name} listening on {bind} (actual {})", listener.local_addr().unwrap());
    for stream in listener.incoming().flatten() {
        let name = name.clone();
        std::thread::spawn(move || {
            if let Err(e) = handle(stream, &name) {
                eprintln!("request error: {e}");
            }
        });
    }
}

fn handle(mut s: TcpStream, name: &str) -> std::io::Result<()> {
    let mut line = String::new();
    let mut r = BufReader::new(s.try_clone()?);
    r.read_line(&mut line)?;
    // drain headers
    loop {
        let mut h = String::new();
        if r.read_line(&mut h)? == 0 || h == "\r\n" || h == "\n" {
            break;
        }
    }
    let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
    println!("{} {}", s.peer_addr()?, line.trim());
    let body = if let Some(rest) = path.strip_prefix("/fetch/") {
        let (hostport, p) = rest.split_once('/').unwrap_or((rest, ""));
        match fetch(hostport, &format!("/{p}")) {
            Ok(b) => format!("{name} fetched {hostport}: {b}"),
            Err(e) => format!("{name} failed to fetch {hostport}: {e}\n"),
        }
    } else {
        format!("hello from {name} local={} peer={}\n", s.local_addr()?, s.peer_addr()?)
    };
    write!(
        s,
        "HTTP/1.0 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn fetch(hostport: &str, path: &str) -> std::io::Result<String> {
    let mut c = TcpStream::connect(hostport)?; // getaddrinfo + connect
    c.set_read_timeout(Some(Duration::from_secs(5)))?;
    write!(c, "GET {path} HTTP/1.0\r\nHost: {hostport}\r\n\r\n")?;
    let mut resp = String::new();
    c.read_to_string(&mut resp)?;
    Ok(resp.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or(resp))
}
