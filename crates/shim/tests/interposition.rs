//! End-to-end tests of the shim: this test binary re-runs itself as a child process
//! with `DYLD_INSERT_LIBRARIES` pointing at the built dylib, and the child performs one
//! scenario through ordinary std networking calls. The child prints `RESULT key=value`
//! lines; the parent asserts on them.
//!
//! Scenarios that need a loopback alias (an address in 127/8 other than 127.0.0.1)
//! skip themselves when there is none; `ncompose system install` creates them.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

const SCENARIO: &str = "NC_E2E_SCENARIO";

// ---------------------------------------------------------------------------
// child side
// ---------------------------------------------------------------------------

/// Runs only in the child (selected with `--exact child`); a no-op in the parent run.
#[test]
fn child() {
    let Ok(scenario) = std::env::var(SCENARIO) else { return };
    let arg = |name: &str| std::env::var(name).unwrap_or_default();
    let result = |k: &str, v: String| {
        println!("RESULT {k}={v}");
        std::io::stdout().flush().unwrap();
    };
    match scenario.as_str() {
        "bind" => {
            let l = TcpListener::bind(arg("NC_BIND")).unwrap();
            result("local", l.local_addr().unwrap().to_string());
            if arg("NC_HOLD") == "1" {
                result("ready", "1".into());
                std::thread::sleep(Duration::from_secs(3));
            }
        }
        "resolve" => {
            for name in arg("NC_NAMES").split(',') {
                let addrs: Vec<String> = match (name, 80).to_socket_addrs() {
                    Ok(a) => a.map(|a| a.ip().to_string()).collect(),
                    Err(e) => vec![format!("error:{e}")],
                };
                result(name, addrs.join("|"));
            }
            let c = std::ffi::CString::new(arg("NC_GHBN")).unwrap();
            let he = unsafe { gethostbyname(c.as_ptr()) };
            let ip = if he.is_null() {
                "null".to_string()
            } else {
                unsafe { Ipv4Addr::from(*((*(*he).h_addr_list) as *const [u8; 4])) }.to_string()
            };
            result("gethostbyname", ip);
        }
        "v6only" => unsafe {
            let fd = libc::socket(libc::AF_INET6, libc::SOCK_STREAM, 0);
            let one: libc::c_int = 1;
            libc::setsockopt(
                fd,
                libc::IPPROTO_IPV6,
                libc::IPV6_V6ONLY,
                &one as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
            let mut a: libc::sockaddr_in6 = std::mem::zeroed();
            a.sin6_len = std::mem::size_of::<libc::sockaddr_in6>() as u8;
            a.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            a.sin6_port = arg("NC_PORT").parse::<u16>().unwrap().to_be();
            let r = libc::bind(
                fd,
                &a as *const _ as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
            );
            let errno = *libc::__error();
            result("bind", format!("{r}"));
            result("errno", format!("{errno}"));
        },
        "dns" => {
            let s = UdpSocket::bind(arg("NC_UDP_BIND")).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            s.send_to(b"query", arg("NC_NAMESERVER")).unwrap();
            let mut buf = [0u8; 64];
            match s.recv_from(&mut buf) {
                Ok((n, from)) => {
                    result("answer", String::from_utf8_lossy(&buf[..n]).into_owned());
                    result("from", from.to_string());
                }
                Err(e) => result("answer", format!("error:{e}")),
            }
        }
        "dns-connected" => {
            // Resolvers that connect() their UDP socket and then use send/recv.
            let s = UdpSocket::bind(arg("NC_UDP_BIND")).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            s.connect(arg("NC_NAMESERVER")).unwrap();
            s.send(b"query").unwrap();
            let mut buf = [0u8; 64];
            match s.recv(&mut buf) {
                Ok(n) => result("answer", String::from_utf8_lossy(&buf[..n]).into_owned()),
                Err(e) => result("answer", format!("error:{e}")),
            }
        }
        "connect" => {
            let mut c = TcpStream::connect(arg("NC_CONNECT")).unwrap();
            result("peer", c.peer_addr().unwrap().to_string());
            result("local", c.local_addr().unwrap().to_string());
            let mut s = String::new();
            c.read_to_string(&mut s).unwrap();
            result("said", s);
        }
        other => panic!("unknown scenario {other}"),
    }
}

extern "C" {
    fn gethostbyname(name: *const libc::c_char) -> *mut libc::hostent;
}

// ---------------------------------------------------------------------------
// parent side
// ---------------------------------------------------------------------------

fn shim() -> PathBuf {
    // The dylib this very `cargo test` built sits beside the test binary in target/<profile>/deps
    // (the shim is also an rlib, so the test depends on it and it is rebuilt). The copy one level
    // up is only refreshed by `cargo build` and may be stale.
    let exe = std::env::current_exe().unwrap();
    let dylib = exe.parent().unwrap().join("libncompose_shim.dylib");
    assert!(dylib.exists(), "{} not built", dylib.display());
    dylib
}

fn spawn(scenario: &str, own_ip: Option<Ipv4Addr>, env: &[(&str, String)]) -> Child {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["--exact", "child", "--nocapture", "--test-threads=1"])
        .env(SCENARIO, scenario)
        .env("DYLD_INSERT_LIBRARIES", shim())
        .env_remove("NCOMPOSE_IP")
        .env_remove("NCOMPOSE_HOSTS")
        .env_remove("NCOMPOSE_DNS")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(ip) = own_ip {
        cmd.env("NCOMPOSE_IP", ip.to_string());
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.spawn().unwrap()
}

fn results(child: Child) -> HashMap<String, String> {
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "child failed:\n{stdout}\n{}", String::from_utf8_lossy(&out.stderr));
    stdout
        .lines()
        // libtest prints "test child ... " without a newline before the first result.
        .filter_map(|l| l.split_once("RESULT ").map(|(_, r)| r))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn run(scenario: &str, own_ip: Option<Ipv4Addr>, env: &[(&str, String)]) -> HashMap<String, String> {
    results(spawn(scenario, own_ip, env))
}

/// A port that was free a moment ago.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// Loopback aliases other than 127.0.0.1 that exist on lo0 right now.
fn aliases() -> Vec<Ipv4Addr> {
    let out = Command::new("/sbin/ifconfig").arg("lo0").output().unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().strip_prefix("inet "))
        .filter_map(|l| l.split_whitespace().next()?.parse::<Ipv4Addr>().ok())
        .filter(|ip| ip.is_loopback() && *ip != Ipv4Addr::LOCALHOST)
        .collect()
}

/// Two aliases, or None (and a note) when this machine has fewer.
fn two_aliases() -> Option<(Ipv4Addr, Ipv4Addr)> {
    let a = aliases();
    // Prefer the end of the pool, which projects allocate last.
    match a.len() {
        n if n >= 2 => Some((a[n - 1], a[n - 2])),
        _ => {
            eprintln!("skipped: needs two loopback aliases (ncompose system install)");
            None
        }
    }
}

const LOCALHOST: Ipv4Addr = Ipv4Addr::LOCALHOST;

#[test]
fn wildcard_bind_lands_on_the_service_address() {
    let port = free_port();
    let r = run("bind", Some(LOCALHOST), &[("NC_BIND", format!("0.0.0.0:{port}"))]);
    assert_eq!(r["local"], format!("127.0.0.1:{port}"));
}

#[test]
fn dual_stack_wildcard_bind_becomes_v4_mapped() {
    let port = free_port();
    let r = run("bind", Some(LOCALHOST), &[("NC_BIND", format!("[::]:{port}"))]);
    assert_eq!(r["local"], format!("[::ffff:127.0.0.1]:{port}"));
}

#[test]
fn ephemeral_wildcard_binds_are_client_sockets_and_left_alone() {
    let r = run("bind", Some(LOCALHOST), &[("NC_BIND", "0.0.0.0:0".into())]);
    assert!(r["local"].starts_with("0.0.0.0:"), "{}", r["local"]);
}

#[test]
fn without_an_address_the_shim_changes_nothing() {
    let port = free_port();
    let r = run("bind", None, &[("NC_BIND", format!("0.0.0.0:{port}"))]);
    assert_eq!(r["local"], format!("0.0.0.0:{port}"));
}

#[test]
fn v6_only_wildcard_binds_are_refused() {
    let r = run("v6only", Some(LOCALHOST), &[("NC_PORT", free_port().to_string())]);
    assert_eq!(r["bind"], "-1");
    assert_eq!(r["errno"], libc::EADDRNOTAVAIL.to_string());
}

#[test]
fn service_names_resolve_and_other_names_pass_through() {
    let r = run(
        "resolve",
        Some(LOCALHOST),
        &[
            ("NCOMPOSE_HOSTS", "db=127.10.0.7,Api=127.10.0.8".into()),
            ("NC_NAMES", "db,API,DB.,localhost".into()),
            ("NC_GHBN", "api".into()),
        ],
    );
    assert_eq!(r["db"], "127.10.0.7");
    assert_eq!(r["API"], "127.10.0.8", "names are case-insensitive");
    assert_eq!(r["DB."], "127.10.0.7", "a trailing dot is the same name");
    assert!(r["localhost"].split('|').any(|ip| ip == "127.0.0.1"), "{}", r["localhost"]);
    assert_eq!(r["gethostbyname"], "127.10.0.8");
}

#[test]
fn dns_datagrams_go_to_the_embedded_resolver_and_answer_as_the_nameserver() {
    let resolver = UdpSocket::bind("127.0.0.1:0").unwrap();
    resolver.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let port = resolver.local_addr().unwrap().port();
    let serve = std::thread::spawn(move || {
        let mut buf = [0u8; 64];
        for _ in 0..2 {
            let (n, from) = resolver.recv_from(&mut buf).unwrap();
            assert_eq!(&buf[..n], b"query");
            resolver.send_to(b"answer", from).unwrap();
        }
    });
    // An IPv4 socket and a dual-stack IPv6 one (Netty's kind), both bound before sending.
    for (bind, nameserver) in [("0.0.0.0:0", "192.0.2.53:53"), ("[::]:0", "[::ffff:192.0.2.53]:53")] {
        let r = run(
            "dns",
            Some(LOCALHOST),
            &[
                ("NCOMPOSE_DNS", port.to_string()),
                ("NC_UDP_BIND", bind.into()),
                ("NC_NAMESERVER", nameserver.into()),
            ],
        );
        assert_eq!(r["answer"], "answer", "{bind}");
        assert_eq!(r["from"], nameserver, "the reply must look like it came from the nameserver asked");
    }
    serve.join().unwrap();
}

#[test]
fn dns_over_a_connected_udp_socket_is_redirected_too() {
    let resolver = UdpSocket::bind("127.0.0.1:0").unwrap();
    resolver.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let port = resolver.local_addr().unwrap().port();
    let serve = std::thread::spawn(move || {
        let mut buf = [0u8; 64];
        let (_, from) = resolver.recv_from(&mut buf).unwrap();
        resolver.send_to(b"answer", from).unwrap();
    });
    let r = run(
        "dns-connected",
        Some(LOCALHOST),
        &[
            ("NCOMPOSE_DNS", port.to_string()),
            ("NC_UDP_BIND", "0.0.0.0:0".into()),
            ("NC_NAMESERVER", "192.0.2.53:53".into()),
        ],
    );
    assert_eq!(r["answer"], "answer");
    serve.join().unwrap();
}

#[test]
fn two_services_bind_the_same_port_on_their_own_addresses() {
    let Some((a, b)) = two_aliases() else { return };
    let port = free_port();
    let bind = [("NC_BIND", format!("0.0.0.0:{port}")), ("NC_HOLD", "1".into())];
    let first = spawn("bind", Some(a), &bind);
    // Give the first one time to bind before the second tries the same port.
    std::thread::sleep(Duration::from_millis(500));
    let second = run("bind", Some(b), &bind);
    let first = results(first);
    assert_eq!(first["local"], format!("{a}:{port}"));
    assert_eq!(second["local"], format!("{b}:{port}"));
}

#[test]
fn localhost_means_the_service_itself() {
    let Some((own, _)) = two_aliases() else { return };
    // Only the service's own address listens; nothing is on 127.0.0.1 at this port.
    let listener = TcpListener::bind((own, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let accept = std::thread::spawn(move || {
        let (mut s, peer) = listener.accept().unwrap();
        s.write_all(b"hello").unwrap();
        peer
    });
    let r = run("connect", Some(own), &[("NC_CONNECT", format!("127.0.0.1:{port}"))]);
    let peer_seen_by_server: SocketAddr = accept.join().unwrap();
    assert_eq!(r["said"], "hello");
    assert_eq!(r["peer"], format!("127.0.0.1:{port}"), "getpeername hides the rewrite");
    assert_eq!(peer_seen_by_server.ip(), own, "the source is the service's own address");
}
