//! Embedded DNS, like Docker's 127.0.0.11: one UDP resolver per service on
//! `<service ip>:<ephemeral port>` (macOS only lets root bind port 53 on a
//! specific address; the shim knows the port from `NCOMPOSE_DNS`). The shim sends a service's DNS datagrams here (for resolvers
//! that bypass `getaddrinfo`, such as Netty's). Names of services that share a
//! network with the asker are answered from its hosts table; everything else is
//! forwarded to the host's nameservers and relayed back unchanged.

use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::watch;

const TYPE_A: u16 = 1;
const TYPE_AAAA: u16 = 28;
const TYPE_ANY: u16 = 255;
const CLASS_IN: u16 = 1;
const TTL_SECS: u32 = 5;
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(3);

/// Bind the resolver for one service on an ephemeral port of its address.
pub async fn bind(ip: Ipv4Addr) -> Result<(UdpSocket, u16)> {
    let socket = UdpSocket::bind(SocketAddrV4::new(ip, 0))
        .await
        .with_context(|| format!("binding {ip}:0/udp"))?;
    let port = socket.local_addr()?.port();
    Ok((socket, port))
}

pub async fn serve(
    socket: UdpSocket,
    hosts: BTreeMap<String, Ipv4Addr>,
    upstreams: Vec<SocketAddr>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let socket = Arc::new(socket);
    let hosts = Arc::new(hosts);
    let upstreams = Arc::new(upstreams);
    let mut buf = vec![0u8; 4096];
    loop {
        tokio::select! {
            _ = shutdown.changed() => return Ok(()),
            r = socket.recv_from(&mut buf) => {
                let Ok((n, from)) = r else { continue };
                let query = buf[..n].to_vec();
                let (socket, hosts, upstreams) = (socket.clone(), hosts.clone(), upstreams.clone());
                tokio::spawn(async move {
                    let reply = match answer_locally(&query, &hosts) {
                        Some(r) => Some(r),
                        None => forward(&query, &upstreams).await,
                    };
                    if let Some(r) = reply {
                        let _ = socket.send_to(&r, from).await;
                    }
                });
            }
        }
    }
}

/// Nameservers from /etc/resolv.conf (macOS keeps it in sync with the system's).
pub fn upstream_nameservers() -> Vec<SocketAddr> {
    parse_resolv_conf(&std::fs::read_to_string("/etc/resolv.conf").unwrap_or_default())
}

fn parse_resolv_conf(text: &str) -> Vec<SocketAddr> {
    text.lines()
        .filter_map(|l| l.trim().strip_prefix("nameserver"))
        .filter_map(|ip| ip.trim().split('%').next()?.parse().ok())
        .map(|ip| SocketAddr::new(ip, 53))
        .collect()
}

async fn forward(query: &[u8], upstreams: &[SocketAddr]) -> Option<Vec<u8>> {
    for up in upstreams {
        let bind: SocketAddr = if up.is_ipv4() { "0.0.0.0:0".parse().ok()? } else { "[::]:0".parse().ok()? };
        let Ok(sock) = UdpSocket::bind(bind).await else { continue };
        if sock.send_to(query, up).await.is_err() {
            continue;
        }
        let mut buf = vec![0u8; 4096];
        if let Ok(Ok((n, _))) = tokio::time::timeout(UPSTREAM_TIMEOUT, sock.recv_from(&mut buf)).await {
            buf.truncate(n);
            return Some(buf);
        }
    }
    None
}

struct Question {
    name: String,
    qtype: u16,
    qclass: u16,
    /// Byte offset just past the question section.
    end: usize,
}

fn parse_question(msg: &[u8]) -> Option<Question> {
    if msg.len() < 12 || u16::from_be_bytes([msg[4], msg[5]]) != 1 {
        return None; // we only handle the usual single-question query
    }
    let mut labels = Vec::new();
    let mut i = 12;
    loop {
        let len = *msg.get(i)? as usize;
        i += 1;
        if len == 0 {
            break;
        }
        if len & 0xc0 != 0 {
            return None; // compression in a query: not worth supporting
        }
        labels.push(String::from_utf8_lossy(msg.get(i..i + len)?).to_ascii_lowercase());
        i += len;
    }
    let qtype = u16::from_be_bytes([*msg.get(i)?, *msg.get(i + 1)?]);
    let qclass = u16::from_be_bytes([*msg.get(i + 2)?, *msg.get(i + 3)?]);
    Some(Question { name: labels.join("."), qtype, qclass, end: i + 4 })
}

/// A reply for a service name, or None to forward the query upstream.
fn answer_locally(query: &[u8], hosts: &BTreeMap<String, Ipv4Addr>) -> Option<Vec<u8>> {
    let q = parse_question(query)?;
    if q.qclass != CLASS_IN {
        return None;
    }
    let ip = *hosts.get(&q.name)?;
    let with_a = matches!(q.qtype, TYPE_A | TYPE_ANY);
    if !with_a && q.qtype != TYPE_AAAA {
        return None;
    }
    let mut r = Vec::with_capacity(q.end + 16);
    r.extend_from_slice(&query[0..2]); // id
    let rd = query[2] & 0x01;
    r.push(0x84 | rd); // QR, AA, RD copied
    r.push(0x80); // RA, NOERROR
    r.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
    r.extend_from_slice(&(with_a as u16).to_be_bytes()); // ANCOUNT (AAAA: none, so clients use A)
    r.extend_from_slice(&[0, 0, 0, 0]); // NSCOUNT, ARCOUNT
    r.extend_from_slice(&query[12..q.end]);
    if with_a {
        r.extend_from_slice(&[0xc0, 0x0c]); // pointer to the question name
        r.extend_from_slice(&TYPE_A.to_be_bytes());
        r.extend_from_slice(&CLASS_IN.to_be_bytes());
        r.extend_from_slice(&TTL_SECS.to_be_bytes());
        r.extend_from_slice(&4u16.to_be_bytes());
        r.extend_from_slice(&ip.octets());
    }
    Some(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(name: &str, qtype: u16) -> Vec<u8> {
        let mut q = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        for label in name.split('.') {
            q.push(label.len() as u8);
            q.extend_from_slice(label.as_bytes());
        }
        q.push(0);
        q.extend_from_slice(&qtype.to_be_bytes());
        q.extend_from_slice(&CLASS_IN.to_be_bytes());
        q
    }

    #[test]
    fn answers_service_names_and_forwards_the_rest() {
        let hosts = BTreeMap::from([("dann-access".to_string(), Ipv4Addr::new(127, 10, 0, 11))]);
        let r = answer_locally(&query("Dann-Access", TYPE_A), &hosts).unwrap();
        assert_eq!(&r[0..2], &[0x12, 0x34]);
        assert_eq!(u16::from_be_bytes([r[6], r[7]]), 1);
        assert_eq!(&r[r.len() - 4..], &[127, 10, 0, 11]);
        let r6 = answer_locally(&query("dann-access", TYPE_AAAA), &hosts).unwrap();
        assert_eq!(u16::from_be_bytes([r6[6], r6[7]]), 0);
        assert!(answer_locally(&query("example.com", TYPE_A), &hosts).is_none());
        assert!(answer_locally(&query("dann-access", 15), &hosts).is_none()); // MX
    }

    #[test]
    fn malformed_queries_are_not_answered() {
        let hosts = BTreeMap::from([("db".to_string(), Ipv4Addr::new(127, 10, 0, 2))]);
        assert!(answer_locally(&[0u8; 5], &hosts).is_none(), "shorter than a header");
        let mut two = query("db", TYPE_A);
        two[5] = 2; // QDCOUNT 2
        assert!(answer_locally(&two, &hosts).is_none());
        let mut compressed = query("db", TYPE_A);
        compressed[12] = 0xc0; // a pointer where a label length should be
        assert!(answer_locally(&compressed, &hosts).is_none());
        let truncated = &query("db", TYPE_A)[..14];
        assert!(answer_locally(truncated, &hosts).is_none());
        let mut chaos = query("db", TYPE_A);
        let n = chaos.len();
        chaos[n - 1] = 3; // class CH
        assert!(answer_locally(&chaos, &hosts).is_none());
    }

    #[test]
    fn any_queries_get_the_a_record_and_flags_follow_the_query() {
        let hosts = BTreeMap::from([("db".to_string(), Ipv4Addr::new(127, 10, 0, 2))]);
        let mut q = query("db", TYPE_ANY);
        q[2] = 0x00; // recursion not desired
        let r = answer_locally(&q, &hosts).unwrap();
        assert_eq!(r[2], 0x84, "QR + AA, RD clear");
        assert_eq!(r[3], 0x80, "RA, NOERROR");
        assert_eq!(&r[r.len() - 4..], &[127, 10, 0, 2]);
        let r = answer_locally(&query("db", TYPE_A), &hosts).unwrap();
        assert_eq!(r[2], 0x85, "RD copied");
        // The answer's name is a pointer to the question, and the question is echoed.
        let q = query("db", TYPE_A);
        assert_eq!(&r[12..q.len()], &q[12..]);
        assert_eq!(&r[q.len()..q.len() + 2], &[0xc0, 0x0c]);
    }

    #[test]
    fn resolv_conf_nameservers() {
        let text = "# generated\nsearch example.org\nnameserver 192.168.1.1\nnameserver fe80::1%en0\n  nameserver   9.9.9.9  \nnameserver bogus\n";
        let servers: Vec<String> = parse_resolv_conf(text).iter().map(|s| s.to_string()).collect();
        assert_eq!(servers, ["192.168.1.1:53", "[fe80::1]:53", "9.9.9.9:53"]);
    }

    #[tokio::test]
    async fn serves_service_names_and_relays_everything_else() {
        // A fake upstream that answers every query with a fixed payload.
        let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 512];
            while let Ok((_, from)) = upstream.recv_from(&mut buf).await {
                let _ = upstream.send_to(b"from-upstream", from).await;
            }
        });

        let (socket, port) = bind(Ipv4Addr::LOCALHOST).await.unwrap();
        let hosts = BTreeMap::from([("db".to_string(), Ipv4Addr::new(127, 10, 0, 2))]);
        let (stop, stopped) = watch::channel(false);
        let server = tokio::spawn(serve(socket, hosts, vec![upstream_addr], stopped));

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client.connect(("127.0.0.1", port)).await.unwrap();
        let mut buf = [0u8; 512];

        client.send(&query("db", TYPE_A)).await.unwrap();
        let n = tokio::time::timeout(Duration::from_secs(2), client.recv(&mut buf)).await.unwrap().unwrap();
        assert_eq!(&buf[n - 4..n], &[127, 10, 0, 2]);

        client.send(&query("example", TYPE_A)).await.unwrap();
        let n = tokio::time::timeout(Duration::from_secs(5), client.recv(&mut buf)).await.unwrap().unwrap();
        assert_eq!(&buf[..n], b"from-upstream");

        stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(2), server).await.unwrap().unwrap().unwrap();
    }
}
