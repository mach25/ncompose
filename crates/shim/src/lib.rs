//! ncompose interposition shim.
//!
//! Loaded into service processes via `DYLD_INSERT_LIBRARIES`. Uses dyld's
//! `__DATA,__interpose` section so that every image in the process that calls
//! `bind`, `connect`, ... gets our replacement, while calls made from *this*
//! image still reach the real libSystem implementations.
//!
//! Configuration (set by the supervisor):
//! - `NCOMPOSE_IP`     – this service's assigned IPv4 address, e.g. `127.10.0.2`
//! - `NCOMPOSE_HOSTS`  – name table, `db=127.10.0.3,api=127.10.0.2`
//! - `NCOMPOSE_DNS`    – port of the supervisor's DNS resolver on `NCOMPOSE_IP`
//! - `NCOMPOSE_DEBUG`  – if set to a non-empty value, log every rewrite to stderr
//!
//! Policy (IPv4-centric; IPv6 wildcard/loopback is mapped onto the IPv4 address
//! via v4-mapped IPv6 addresses on dual-stack sockets; binds on explicitly
//! v6-only sockets fail with EADDRNOTAVAIL):
//! - `bind`: 0.0.0.0 / 127.0.0.1 / :: / ::1 -> own IP (port unchanged). A wildcard
//!   bind to port 0 is a client socket and is left alone, so it can still reach the
//!   outside world.
//! - `connect`: 127.0.0.1 / ::1 -> own IP ("localhost means me"); unbound sockets
//!   connecting to 127/8 get their source bound to own IP.
//! - `sendto` / `sendmsg`: same destination rewrite as `connect`
//! - `getaddrinfo` / `gethostbyname`: service names -> assigned IPs
//! - `getpeername` own IP -> 127.0.0.1 (hide the localhost rewrite)
//! - DNS (with `NCOMPOSE_DNS`): UDP datagrams to port 53 of a non-service address go to
//!   the supervisor's resolver at `NCOMPOSE_IP:NCOMPOSE_DNS` instead, like Docker's embedded DNS,
//!   for resolvers that bypass `getaddrinfo` (Netty, c-ares, ...). `recvfrom` /
//!   `recvmsg` report the answer as coming from the nameserver that was asked (matched by
//!   DNS transaction ID, so one socket may query several).
//! - `close`: forgets the descriptor's DNS bookkeeping, as its number gets reused.

#![allow(clippy::missing_safety_doc)]

use libc::{
    addrinfo, c_char, c_int, c_void, hostent, in6_addr, in_addr, msghdr, sa_family_t, sockaddr,
    sockaddr_in, sockaddr_in6, socklen_t, ssize_t, AF_INET, AF_INET6,
};
use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::mem::{size_of, zeroed};
use std::net::Ipv4Addr;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

// ---------------------------------------------------------------------------
// dyld interposing
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct Interpose {
    replacement: *const (),
    original: *const (),
}
unsafe impl Sync for Interpose {}

macro_rules! interpose {
    ($slot:ident: $orig:path => $repl:path) => {
        #[used]
        #[link_section = "__DATA,__interpose"]
        static $slot: Interpose = Interpose {
            replacement: $repl as *const (),
            original: $orig as *const (),
        };
    };
}

interpose!(I_BIND: libc::bind => nc_bind);
interpose!(I_CONNECT: libc::connect => nc_connect);
interpose!(I_SENDTO: libc::sendto => nc_sendto);
interpose!(I_SENDMSG: libc::sendmsg => nc_sendmsg);
interpose!(I_GETPEERNAME: libc::getpeername => nc_getpeername);
interpose!(I_RECVFROM: libc::recvfrom => nc_recvfrom);
interpose!(I_RECVMSG: libc::recvmsg => nc_recvmsg);
interpose!(I_GETADDRINFO: libc::getaddrinfo => nc_getaddrinfo);
interpose!(I_GETHOSTBYNAME: gethostbyname => nc_gethostbyname);
interpose!(I_CLOSE: real_close => nc_close);
interpose!(I_CLOSE_NOCANCEL: real_close_nocancel => nc_close_nocancel);

extern "C" {
    fn gethostbyname(name: *const c_char) -> *mut hostent;
    #[link_name = "close"]
    fn real_close(fd: c_int) -> c_int;
    #[link_name = "close$NOCANCEL"]
    fn real_close_nocancel(fd: c_int) -> c_int;
}

// ---------------------------------------------------------------------------
// configuration
// ---------------------------------------------------------------------------

struct Config {
    ip: Option<Ipv4Addr>,
    hosts: Vec<(String, Ipv4Addr)>,
    dns: Option<u16>,
    debug: bool,
}

static CONFIG: OnceLock<Config> = OnceLock::new();

fn env(name: &CStr) -> Option<String> {
    // libc::getenv avoids std's env lock; we may be called very early.
    let p = unsafe { libc::getenv(name.as_ptr()) };
    if p.is_null() {
        return None;
    }
    Some(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
}

fn config() -> &'static Config {
    CONFIG.get_or_init(|| {
        let ip = env(c"NCOMPOSE_IP").and_then(|s| s.trim().parse().ok());
        let hosts = env(c"NCOMPOSE_HOSTS").map(|s| parse_hosts(&s)).unwrap_or_default();
        let debug = env(c"NCOMPOSE_DEBUG").is_some_and(|s| !s.is_empty() && s != "0");
        let dns = env(c"NCOMPOSE_DNS").and_then(|s| s.trim().parse().ok()).filter(|p| *p != 0);
        Config { ip, hosts, dns, debug }
    })
}

/// `db=127.10.0.3,api=127.10.0.2` (`;` also separates); names are case-insensitive,
/// malformed or nameless entries are skipped.
fn parse_hosts(s: &str) -> Vec<(String, Ipv4Addr)> {
    s.split([',', ';'])
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            let name = k.trim().to_ascii_lowercase();
            (!name.is_empty()).then_some(())?;
            Some((name, v.trim().parse().ok()?))
        })
        .collect()
}

/// Log to stderr when `NCOMPOSE_DEBUG` is set. The arguments are only evaluated
/// then: these calls sit on every interposed bind and connect.
macro_rules! debug {
    ($($arg:tt)*) => {
        if config().debug {
            write_debug(format_args!($($arg)*));
        }
    };
}

fn write_debug(msg: std::fmt::Arguments) {
    let s = format!("[ncompose-shim pid={}] {}\n", std::process::id(), msg);
    unsafe { libc::write(2, s.as_ptr() as *const c_void, s.len()) };
}

fn lookup(name: &str) -> Option<Ipv4Addr> {
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    config()
        .hosts
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, ip)| *ip)
}

// ---------------------------------------------------------------------------
// sockaddr helpers
// ---------------------------------------------------------------------------

const LOOPBACK4: Ipv4Addr = Ipv4Addr::LOCALHOST;

fn v4_from(a: in_addr) -> Ipv4Addr {
    Ipv4Addr::from(u32::from_be(a.s_addr))
}
fn in_addr_from(ip: Ipv4Addr) -> in_addr {
    in_addr { s_addr: u32::from(ip).to_be() }
}
fn mapped(ip: Ipv4Addr) -> in6_addr {
    let mut b = [0u8; 16];
    b[10] = 0xff;
    b[11] = 0xff;
    b[12..].copy_from_slice(&ip.octets());
    in6_addr { s6_addr: b }
}
/// Interpret an IPv6 address as "IPv4-ish": v4-mapped -> that v4, `::1` -> 127.0.0.1,
/// `::` -> 0.0.0.0. Anything else -> None.
fn v6_as_v4(a: &in6_addr) -> Option<Ipv4Addr> {
    let b = a.s6_addr;
    if b[..10] == [0; 10] && b[10] == 0xff && b[11] == 0xff {
        return Some(Ipv4Addr::new(b[12], b[13], b[14], b[15]));
    }
    if b[..15] == [0; 15] {
        return match b[15] {
            0 => Some(Ipv4Addr::UNSPECIFIED),
            1 => Some(LOOPBACK4),
            _ => None,
        };
    }
    None
}

/// An owned socket address we can hand to the real syscall.
enum Addr {
    V4(sockaddr_in),
    V6(sockaddr_in6),
}
impl Addr {
    fn ptr(&self) -> *const sockaddr {
        match self {
            Addr::V4(a) => a as *const _ as *const sockaddr,
            Addr::V6(a) => a as *const _ as *const sockaddr,
        }
    }
    fn len(&self) -> socklen_t {
        match self {
            Addr::V4(_) => size_of::<sockaddr_in>() as socklen_t,
            Addr::V6(_) => size_of::<sockaddr_in6>() as socklen_t,
        }
    }
}

unsafe fn family(addr: *const sockaddr, len: socklen_t) -> Option<c_int> {
    if addr.is_null() || (len as usize) < 2 {
        return None;
    }
    Some((*addr).sa_family as c_int)
}

/// Make sure v4-mapped addresses can be used on this socket. Dual-stack is the
/// default; the option can only be cleared before the socket is bound, so it is
/// touched only when the app made the socket v6-only.
unsafe fn allow_v4_mapped(fd: c_int) -> bool {
    if !is_v6only(fd) {
        return true;
    }
    let zero: c_int = 0;
    libc::setsockopt(
        fd,
        libc::IPPROTO_IPV6,
        libc::IPV6_V6ONLY,
        &zero as *const _ as *const c_void,
        size_of::<c_int>() as socklen_t,
    ) == 0
}

unsafe fn is_v6only(fd: c_int) -> bool {
    let mut v: c_int = 0;
    let mut l = size_of::<c_int>() as socklen_t;
    libc::getsockopt(fd, libc::IPPROTO_IPV6, libc::IPV6_V6ONLY, &mut v as *mut _ as *mut c_void, &mut l) == 0
        && v != 0
}

/// Rewrite `addr` so that any address in `from` becomes `to`.
/// Returns None if no rewrite applies.
unsafe fn rewrite(
    fd: c_int,
    addr: *const sockaddr,
    len: socklen_t,
    matches: impl Fn(Ipv4Addr) -> bool,
    to: Ipv4Addr,
) -> Option<Addr> {
    match family(addr, len)? {
        AF_INET if len as usize >= size_of::<sockaddr_in>() => {
            let mut a = *(addr as *const sockaddr_in);
            if !matches(v4_from(a.sin_addr)) {
                return None;
            }
            a.sin_addr = in_addr_from(to);
            Some(Addr::V4(a))
        }
        AF_INET6 if len as usize >= size_of::<sockaddr_in6>() => {
            let mut a = *(addr as *const sockaddr_in6);
            if !matches(v6_as_v4(&a.sin6_addr)?) {
                return None;
            }
            if fd >= 0 && !allow_v4_mapped(fd) {
                return None;
            }
            a.sin6_addr = mapped(to);
            Some(Addr::V6(a))
        }
        _ => None,
    }
}

fn fmt_addr(addr: &Addr) -> String {
    match addr {
        Addr::V4(a) => format!("{}:{}", v4_from(a.sin_addr), u16::from_be(a.sin_port)),
        Addr::V6(a) => format!(
            "[{}]:{}",
            std::net::Ipv6Addr::from(a.sin6_addr.s6_addr),
            u16::from_be(a.sin6_port)
        ),
    }
}

unsafe fn fmt_raw(addr: *const sockaddr, len: socklen_t) -> String {
    match family(addr, len) {
        Some(AF_INET) => fmt_addr(&Addr::V4(*(addr as *const sockaddr_in))),
        Some(AF_INET6) => fmt_addr(&Addr::V6(*(addr as *const sockaddr_in6))),
        _ => "?".into(),
    }
}

fn is_wild_or_loopback(ip: Ipv4Addr) -> bool {
    ip.is_unspecified() || ip == LOOPBACK4
}
fn is_loopback(ip: Ipv4Addr) -> bool {
    ip == LOOPBACK4
}

/// If the socket is not yet bound and is about to talk to a loopback address,
/// bind its source to our IP so peers see who we are.
unsafe fn bind_source_if_unbound(fd: c_int, dest: &Addr, own: Ipv4Addr) {
    let dest_v4 = match dest {
        Addr::V4(a) => v4_from(a.sin_addr),
        Addr::V6(a) => match v6_as_v4(&a.sin6_addr) {
            Some(ip) => ip,
            None => return,
        },
    };
    if !dest_v4.is_loopback() {
        return;
    }
    let mut ss: libc::sockaddr_storage = zeroed();
    let mut sl = size_of::<libc::sockaddr_storage>() as socklen_t;
    if libc::getsockname(fd, &mut ss as *mut _ as *mut sockaddr, &mut sl) != 0 {
        return;
    }
    let src = match ss.ss_family as c_int {
        AF_INET => {
            let a = &*(&ss as *const _ as *const sockaddr_in);
            if a.sin_port != 0 || a.sin_addr.s_addr != 0 {
                return;
            }
            let mut s: sockaddr_in = zeroed();
            s.sin_len = size_of::<sockaddr_in>() as u8;
            s.sin_family = AF_INET as sa_family_t;
            s.sin_addr = in_addr_from(own);
            Addr::V4(s)
        }
        AF_INET6 => {
            let a = &*(&ss as *const _ as *const sockaddr_in6);
            if a.sin6_port != 0 || a.sin6_addr.s6_addr != [0; 16] {
                return;
            }
            let mut s: sockaddr_in6 = zeroed();
            s.sin6_len = size_of::<sockaddr_in6>() as u8;
            s.sin6_family = AF_INET6 as sa_family_t;
            s.sin6_addr = mapped(own);
            Addr::V6(s)
        }
        _ => return,
    };
    if libc::bind(fd, src.ptr(), src.len()) == 0 {
        debug!("fd {fd}: source bound to {}", fmt_addr(&src));
    }
}

unsafe fn port_of(addr: *const sockaddr, len: socklen_t) -> Option<u16> {
    match family(addr, len)? {
        AF_INET if len as usize >= size_of::<sockaddr_in>() => {
            Some(u16::from_be((*(addr as *const sockaddr_in)).sin_port))
        }
        AF_INET6 if len as usize >= size_of::<sockaddr_in6>() => {
            Some(u16::from_be((*(addr as *const sockaddr_in6)).sin6_port))
        }
        _ => None,
    }
}

unsafe fn is_wildcard(addr: *const sockaddr, len: socklen_t) -> bool {
    match family(addr, len) {
        Some(AF_INET) if len as usize >= size_of::<sockaddr_in>() => {
            (*(addr as *const sockaddr_in)).sin_addr.s_addr == 0
        }
        Some(AF_INET6) if len as usize >= size_of::<sockaddr_in6>() => {
            (*(addr as *const sockaddr_in6)).sin6_addr.s6_addr == [0; 16]
        }
        _ => false,
    }
}

// --- DNS redirection ---------------------------------------------------------

const DNS_PORT: u16 = 53;

/// A nameserver address as the app gave it (raw sockaddr bytes).
type RawAddr = Vec<u8>;

/// Remembered queries per socket: which nameserver each DNS transaction ID was sent
/// to, so a reply is reported as coming from the nameserver that query asked, even
/// when one socket queries several (Netty retries and rotates on one channel).
#[derive(Default)]
struct Asked {
    /// (transaction ID, nameserver), oldest first; bounded.
    by_id: std::collections::VecDeque<(u16, RawAddr)>,
    /// The nameserver of the latest query, for replies whose ID we never saw
    /// (connected sockets send with `send`, which is not interposed).
    last: RawAddr,
}

const ASKED_PER_FD: usize = 64;

static DNS_ASKED: Mutex<Option<HashMap<c_int, Asked>>> = Mutex::new(None);
/// Set once any query was redirected; until then `close` skips the lock.
static DNS_USED: AtomicBool = AtomicBool::new(false);

fn dns_asked() -> std::sync::MutexGuard<'static, Option<HashMap<c_int, Asked>>> {
    DNS_ASKED.lock().unwrap_or_else(|e| e.into_inner())
}

/// The transaction ID at the start of a DNS message.
fn dns_id(msg: &[u8]) -> Option<u16> {
    Some(u16::from_be_bytes([*msg.first()?, *msg.get(1)?]))
}

/// The first two bytes of a scatter/gather buffer, limited to `total` bytes.
unsafe fn iov_id(iov: *const libc::iovec, iovlen: c_int, mut total: usize) -> Option<u16> {
    if iov.is_null() {
        return None;
    }
    let mut head = [0u8; 2];
    let mut have = 0;
    for i in 0..iovlen.max(0) as usize {
        let v = &*iov.add(i);
        let n = v.iov_len.min(total).min(2 - have);
        if n > 0 && !v.iov_base.is_null() {
            std::ptr::copy_nonoverlapping(v.iov_base as *const u8, head.as_mut_ptr().add(have), n);
        }
        have += n;
        total -= n.min(total);
        if have == 2 {
            return dns_id(&head);
        }
    }
    None
}

fn remember_query(fd: c_int, id: Option<u16>, raw: RawAddr) {
    DNS_USED.store(true, Ordering::Relaxed);
    let mut guard = dns_asked();
    let asked = guard.get_or_insert_with(HashMap::new).entry(fd).or_default();
    if let Some(id) = id {
        asked.by_id.retain(|(i, _)| *i != id);
        if asked.by_id.len() == ASKED_PER_FD {
            asked.by_id.pop_front();
        }
        asked.by_id.push_back((id, raw.clone()));
    }
    asked.last = raw;
}

/// The nameserver a reply on `fd` with transaction `id` answers.
fn asked_nameserver(fd: c_int, id: Option<u16>) -> Option<RawAddr> {
    let guard = dns_asked();
    let asked = guard.as_ref()?.get(&fd)?;
    id.and_then(|id| asked.by_id.iter().find(|(i, _)| *i == id).map(|(_, a)| a.clone()))
        .or_else(|| (!asked.last.is_empty()).then(|| asked.last.clone()))
}

fn forget_fd(fd: c_int) {
    if DNS_USED.load(Ordering::Relaxed) {
        if let Some(m) = dns_asked().as_mut() {
            m.remove(&fd);
        }
    }
}

unsafe fn is_datagram(fd: c_int) -> bool {
    let mut t: c_int = 0;
    let mut l = size_of::<c_int>() as socklen_t;
    libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_TYPE, &mut t as *mut _ as *mut c_void, &mut l) == 0
        && t == libc::SOCK_DGRAM
}

/// A DNS query to an outside nameserver goes to our supervisor's resolver instead.
/// Service addresses (127/8 other than 127.0.0.1) are left alone: a service may run DNS.
/// `id` is the query's DNS transaction ID, when the datagram is at hand.
unsafe fn dns_redirect(
    fd: c_int,
    addr: *const sockaddr,
    len: socklen_t,
    own: Ipv4Addr,
    id: Option<u16>,
) -> Option<Addr> {
    let dns_port = config().dns?;
    if port_of(addr, len)? != DNS_PORT || !is_datagram(fd) {
        return None;
    }
    let target = |ip: Ipv4Addr| !(ip.is_loopback() && ip != LOOPBACK4);
    let new = match family(addr, len)? {
        AF_INET => {
            let mut a = *(addr as *const sockaddr_in);
            if !target(v4_from(a.sin_addr)) {
                return None;
            }
            a.sin_addr = in_addr_from(own);
            a.sin_port = dns_port.to_be();
            Addr::V4(a)
        }
        AF_INET6 => {
            let mut a = *(addr as *const sockaddr_in6);
            if v6_as_v4(&a.sin6_addr).is_some_and(|ip| !target(ip)) || !allow_v4_mapped(fd) {
                return None;
            }
            a.sin6_addr = mapped(own);
            a.sin6_port = dns_port.to_be();
            a.sin6_scope_id = 0;
            Addr::V6(a)
        }
        _ => return None,
    };
    remember_query(fd, id, std::slice::from_raw_parts(addr as *const u8, len as usize).to_vec());
    debug!("dns fd {fd}: {} -> {}", fmt_raw(addr, len), fmt_addr(&new));
    Some(new)
}

/// If a datagram on `fd` came from our resolver, report the nameserver that was asked
/// (by the query with the reply's transaction ID `id`).
unsafe fn dns_unredirect(fd: c_int, addr: *mut sockaddr, len: *mut socklen_t, id: Option<u16>) {
    let Some(own) = config().ip else { return };
    let Some(dns_port) = config().dns else { return };
    if addr.is_null() || len.is_null() || port_of(addr, *len) != Some(dns_port) {
        return;
    }
    let from_us = match family(addr, *len) {
        Some(AF_INET) => v4_from((*(addr as *const sockaddr_in)).sin_addr) == own,
        Some(AF_INET6) => v6_as_v4(&(*(addr as *const sockaddr_in6)).sin6_addr) == Some(own),
        _ => false,
    };
    if !from_us {
        return;
    }
    if let Some(raw) = asked_nameserver(fd, id) {
        if *len as usize >= raw.len() {
            std::ptr::copy_nonoverlapping(raw.as_ptr(), addr as *mut u8, raw.len());
            *len = raw.len() as socklen_t;
        }
    }
}

// ---------------------------------------------------------------------------
// interposed calls
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn nc_bind(fd: c_int, addr: *const sockaddr, len: socklen_t) -> c_int {
    let Some(own) = config().ip else {
        return libc::bind(fd, addr, len);
    };
    // An app that explicitly made an IPv6 socket v6-only (e.g. PostgreSQL, nginx)
    // also opens a separate IPv4 socket. We have no per-service IPv6 address, so
    // refuse the v6 bind like a host without IPv6 would; the IPv4 socket gets ours.
    if family(addr, len) == Some(AF_INET6) && len as usize >= size_of::<sockaddr_in6>() {
        let a = &*(addr as *const sockaddr_in6);
        if v6_as_v4(&a.sin6_addr).is_some_and(is_wild_or_loopback) && is_v6only(fd) {
            debug!("bind fd {fd}: {} refused (v6-only socket)", fmt_raw(addr, len));
            *libc::__error() = libc::EADDRNOTAVAIL;
            return -1;
        }
    }
    // A wildcard bind to port 0 is a client socket (NIO and Netty bind before they
    // send); pinning it to our loopback address would cut it off from the outside.
    if port_of(addr, len) == Some(0) && is_wildcard(addr, len) {
        return libc::bind(fd, addr, len);
    }
    match rewrite(fd, addr, len, is_wild_or_loopback, own) {
        Some(new) => {
            // The supervisor's port forwarders may hold a wildcard bind on the same
            // port; BSD only lets a specific bind coexist with it if SO_REUSEADDR is set.
            let one: c_int = 1;
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_REUSEADDR,
                &one as *const _ as *const c_void,
                size_of::<c_int>() as socklen_t,
            );
            let r = libc::bind(fd, new.ptr(), new.len());
            debug!(
                "bind fd {fd}: {} -> {} = {r}",
                fmt_raw(addr, len),
                fmt_addr(&new)
            );
            r
        }
        None => libc::bind(fd, addr, len),
    }
}

#[no_mangle]
pub unsafe extern "C" fn nc_connect(fd: c_int, addr: *const sockaddr, len: socklen_t) -> c_int {
    let Some(own) = config().ip else {
        return libc::connect(fd, addr, len);
    };
    if let Some(dns) = dns_redirect(fd, addr, len, own, None) {
        return libc::connect(fd, dns.ptr(), dns.len());
    }
    let new = rewrite(fd, addr, len, is_loopback, own);
    if let Some(n) = &new {
        debug!(
            "connect fd {fd}: {} -> {}",
            fmt_raw(addr, len),
            fmt_addr(n)
        );
    }
    // Copy the (possibly rewritten) destination so we can inspect it.
    let dest = match &new {
        Some(n) => Some(match n {
            Addr::V4(a) => Addr::V4(*a),
            Addr::V6(a) => Addr::V6(*a),
        }),
        None => match family(addr, len) {
            Some(AF_INET) if len as usize >= size_of::<sockaddr_in>() => {
                Some(Addr::V4(*(addr as *const sockaddr_in)))
            }
            Some(AF_INET6) if len as usize >= size_of::<sockaddr_in6>() => {
                Some(Addr::V6(*(addr as *const sockaddr_in6)))
            }
            _ => None,
        },
    };
    if let Some(d) = &dest {
        bind_source_if_unbound(fd, d, own);
    }
    match new {
        Some(n) => libc::connect(fd, n.ptr(), n.len()),
        None => libc::connect(fd, addr, len),
    }
}

#[no_mangle]
pub unsafe extern "C" fn nc_sendto(
    fd: c_int,
    buf: *const c_void,
    n: libc::size_t,
    flags: c_int,
    addr: *const sockaddr,
    len: socklen_t,
) -> ssize_t {
    if let Some(own) = config().ip {
        let id = if buf.is_null() { None } else { dns_id(std::slice::from_raw_parts(buf as *const u8, n.min(2))) };
        if let Some(new) = dns_redirect(fd, addr, len, own, id).or_else(|| rewrite(fd, addr, len, is_loopback, own)) {
            return libc::sendto(fd, buf, n, flags, new.ptr(), new.len());
        }
    }
    libc::sendto(fd, buf, n, flags, addr, len)
}

#[no_mangle]
pub unsafe extern "C" fn nc_sendmsg(fd: c_int, msg: *const msghdr, flags: c_int) -> ssize_t {
    if let (Some(own), false) = (config().ip, msg.is_null()) {
        let m = &*msg;
        let (name, namelen) = (m.msg_name as *const sockaddr, m.msg_namelen);
        let id = iov_id(m.msg_iov, m.msg_iovlen, usize::MAX);
        if let Some(new) =
            dns_redirect(fd, name, namelen, own, id).or_else(|| rewrite(fd, name, namelen, is_loopback, own))
        {
            let mut copy = *m;
            copy.msg_name = new.ptr() as *mut c_void;
            copy.msg_namelen = new.len();
            return libc::sendmsg(fd, &copy, flags);
        }
    }
    libc::sendmsg(fd, msg, flags)
}

#[no_mangle]
pub unsafe extern "C" fn nc_recvfrom(
    fd: c_int,
    buf: *mut c_void,
    n: libc::size_t,
    flags: c_int,
    addr: *mut sockaddr,
    len: *mut socklen_t,
) -> ssize_t {
    let r = libc::recvfrom(fd, buf, n, flags, addr, len);
    if r >= 0 {
        let id = if buf.is_null() { None } else { dns_id(std::slice::from_raw_parts(buf as *const u8, (r as usize).min(n).min(2))) };
        dns_unredirect(fd, addr, len, id);
    }
    r
}

#[no_mangle]
pub unsafe extern "C" fn nc_recvmsg(fd: c_int, msg: *mut msghdr, flags: c_int) -> ssize_t {
    let r = libc::recvmsg(fd, msg, flags);
    if r >= 0 && !msg.is_null() {
        let m = &mut *msg;
        let id = iov_id(m.msg_iov, m.msg_iovlen, r as usize);
        dns_unredirect(fd, m.msg_name as *mut sockaddr, &mut m.msg_namelen, id);
    }
    r
}

/// A closed descriptor's number is reused by the next socket; drop what we remembered.
#[no_mangle]
pub unsafe extern "C" fn nc_close(fd: c_int) -> c_int {
    forget_fd(fd);
    real_close(fd)
}

#[no_mangle]
pub unsafe extern "C" fn nc_close_nocancel(fd: c_int) -> c_int {
    forget_fd(fd);
    real_close_nocancel(fd)
}

#[no_mangle]
pub unsafe extern "C" fn nc_getpeername(
    fd: c_int,
    addr: *mut sockaddr,
    len: *mut socklen_t,
) -> c_int {
    let r = libc::getpeername(fd, addr, len);
    let Some(own) = config().ip else { return r };
    if r != 0 || addr.is_null() || len.is_null() {
        return r;
    }
    // Only rewrite if the full struct was returned to the caller.
    match family(addr, *len) {
        Some(AF_INET) if *len as usize >= size_of::<sockaddr_in>() => {
            let a = &mut *(addr as *mut sockaddr_in);
            if v4_from(a.sin_addr) == own {
                a.sin_addr = in_addr_from(LOOPBACK4);
            }
        }
        Some(AF_INET6) if *len as usize >= size_of::<sockaddr_in6>() => {
            let a = &mut *(addr as *mut sockaddr_in6);
            if v6_as_v4(&a.sin6_addr) == Some(own) {
                a.sin6_addr = mapped(LOOPBACK4);
            }
        }
        _ => {}
    }
    r
}

#[no_mangle]
pub unsafe extern "C" fn nc_getaddrinfo(
    node: *const c_char,
    service: *const c_char,
    hints: *const addrinfo,
    res: *mut *mut addrinfo,
) -> c_int {
    if !node.is_null() {
        if let Ok(name) = CStr::from_ptr(node).to_str() {
            if let Some(ip) = lookup(name) {
                let mut h: addrinfo = if hints.is_null() { zeroed() } else { *hints };
                h.ai_flags |= libc::AI_NUMERICHOST;
                if h.ai_family == AF_INET6 {
                    h.ai_flags |= libc::AI_V4MAPPED;
                }
                // CString::new cannot fail: an IP string has no NULs.
                let ip_c = CString::new(ip.to_string()).unwrap_or_default();
                let r = libc::getaddrinfo(ip_c.as_ptr(), service, &h, res);
                debug!("getaddrinfo {name} -> {ip} = {r}");
                return r;
            }
        }
    }
    libc::getaddrinfo(node, service, hints, res)
}

struct HostentBuf {
    he: hostent,
    name: CString,
    addr: [u8; 4],
    addr_list: [*mut c_char; 2],
    aliases: [*mut c_char; 1],
}

thread_local! {
    static HOSTENT: RefCell<Option<Box<HostentBuf>>> = const { RefCell::new(None) };
}

#[no_mangle]
pub unsafe extern "C" fn nc_gethostbyname(name: *const c_char) -> *mut hostent {
    if !name.is_null() {
        if let Ok(n) = CStr::from_ptr(name).to_str() {
            if let Some(ip) = lookup(n) {
                debug!("gethostbyname {n} -> {ip}");
                return HOSTENT.with(|cell| {
                    let mut slot = cell.borrow_mut();
                    let mut b = Box::new(HostentBuf {
                        he: zeroed(),
                        name: CString::new(n).unwrap_or_default(),
                        addr: ip.octets(),
                        addr_list: [std::ptr::null_mut(); 2],
                        aliases: [std::ptr::null_mut(); 1],
                    });
                    b.addr_list[0] = b.addr.as_mut_ptr() as *mut c_char;
                    b.he.h_name = b.name.as_ptr() as *mut c_char;
                    b.he.h_aliases = b.aliases.as_mut_ptr();
                    b.he.h_addrtype = AF_INET;
                    b.he.h_length = 4;
                    b.he.h_addr_list = b.addr_list.as_mut_ptr();
                    let p = &mut b.he as *mut hostent;
                    *slot = Some(b);
                    p
                });
            }
        }
    }
    gethostbyname(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn v6_classification() {
        assert_eq!(v6_as_v4(&in6_addr { s6_addr: [0; 16] }), Some(Ipv4Addr::UNSPECIFIED));
        let mut one = [0u8; 16];
        one[15] = 1;
        assert_eq!(v6_as_v4(&in6_addr { s6_addr: one }), Some(LOOPBACK4));
        let ip = Ipv4Addr::new(127, 10, 0, 2);
        assert_eq!(v6_as_v4(&mapped(ip)), Some(ip));
    }

    fn v4(ip: [u8; 4], port: u16) -> sockaddr_in {
        let mut a: sockaddr_in = unsafe { zeroed() };
        a.sin_len = size_of::<sockaddr_in>() as u8;
        a.sin_family = AF_INET as sa_family_t;
        a.sin_port = port.to_be();
        a.sin_addr = in_addr_from(Ipv4Addr::from(ip));
        a
    }

    fn v6(addr: in6_addr, port: u16) -> sockaddr_in6 {
        let mut a: sockaddr_in6 = unsafe { zeroed() };
        a.sin6_len = size_of::<sockaddr_in6>() as u8;
        a.sin6_family = AF_INET6 as sa_family_t;
        a.sin6_port = port.to_be();
        a.sin6_addr = addr;
        a
    }

    fn raw<T>(a: &T) -> (*const sockaddr, socklen_t) {
        (a as *const T as *const sockaddr, size_of::<T>() as socklen_t)
    }

    const OWN: Ipv4Addr = Ipv4Addr::new(127, 10, 0, 2);

    #[test]
    fn dns_ids_are_read_across_buffers() {
        assert_eq!(dns_id(&[0x12, 0x34, 0]), Some(0x1234));
        assert_eq!(dns_id(&[0x12]), None);
        let (a, b) = ([0x12u8], [0x34u8, 0x56]);
        let iov = [
            libc::iovec { iov_base: a.as_ptr() as *mut c_void, iov_len: 1 },
            libc::iovec { iov_base: b.as_ptr() as *mut c_void, iov_len: 2 },
        ];
        unsafe {
            assert_eq!(iov_id(iov.as_ptr(), 2, usize::MAX), Some(0x1234));
            assert_eq!(iov_id(iov.as_ptr(), 2, 1), None, "only one byte was received");
            assert_eq!(iov_id(iov.as_ptr(), 1, usize::MAX), None);
            assert_eq!(iov_id(std::ptr::null(), 2, usize::MAX), None);
        }
    }

    #[test]
    fn nameservers_are_remembered_per_query_and_forgotten_on_close() {
        let fd = -42; // never a real descriptor
        remember_query(fd, Some(1), vec![1]);
        remember_query(fd, Some(2), vec![2]);
        assert_eq!(asked_nameserver(fd, Some(1)), Some(vec![1]));
        assert_eq!(asked_nameserver(fd, Some(2)), Some(vec![2]));
        assert_eq!(asked_nameserver(fd, Some(9)), Some(vec![2]), "unknown ids: the latest");
        assert_eq!(asked_nameserver(fd, None), Some(vec![2]));
        for id in 0..ASKED_PER_FD as u16 + 10 {
            remember_query(fd, Some(100 + id), vec![3]);
        }
        assert_eq!(dns_asked().as_ref().unwrap()[&fd].by_id.len(), ASKED_PER_FD, "bounded");
        forget_fd(fd);
        assert_eq!(asked_nameserver(fd, Some(1)), None);
    }

    #[test]
    fn hosts_table_parsing() {
        let hosts = parse_hosts("db=127.10.0.3, Api = 127.10.0.2;broken;x=not-an-ip;=127.0.0.9");
        assert_eq!(
            hosts,
            [
                ("db".to_string(), Ipv4Addr::new(127, 10, 0, 3)),
                ("api".to_string(), Ipv4Addr::new(127, 10, 0, 2)),
            ]
        );
        assert!(parse_hosts("").is_empty());
    }

    #[test]
    fn address_predicates() {
        assert!(is_wild_or_loopback(Ipv4Addr::UNSPECIFIED) && is_wild_or_loopback(LOOPBACK4));
        assert!(!is_wild_or_loopback(OWN));
        assert!(is_loopback(LOOPBACK4) && !is_loopback(Ipv4Addr::new(127, 0, 0, 2)));
        unsafe {
            let (p, l) = raw(&v4([0, 0, 0, 0], 0));
            assert!(is_wildcard(p, l));
            assert_eq!(port_of(p, l), Some(0));
            let (p, l) = raw(&v6(in6_addr { s6_addr: [0; 16] }, 8080));
            assert!(is_wildcard(p, l));
            assert_eq!(port_of(p, l), Some(8080));
            let (p, l) = raw(&v4([127, 0, 0, 1], 53));
            assert!(!is_wildcard(p, l));
            // A truncated sockaddr is never read past its length.
            assert_eq!(port_of(p, 4), None);
            assert_eq!(family(p, 1), None);
            assert_eq!(family(std::ptr::null(), 16), None);
        }
    }

    #[test]
    fn bind_rewrites_wildcard_and_loopback_to_our_address() {
        unsafe {
            for from in [[0, 0, 0, 0], [127, 0, 0, 1]] {
                let (p, l) = raw(&v4(from, 8080));
                let Some(Addr::V4(new)) = rewrite(-1, p, l, is_wild_or_loopback, OWN) else {
                    panic!("{from:?} should be rewritten");
                };
                assert_eq!(v4_from(new.sin_addr), OWN);
                assert_eq!(u16::from_be(new.sin_port), 8080, "the port is kept");
            }
            let (p, l) = raw(&v4([10, 1, 2, 3], 80));
            assert!(rewrite(-1, p, l, is_wild_or_loopback, OWN).is_none(), "a real address is left alone");
        }
    }

    #[test]
    fn ipv6_wildcard_and_loopback_become_v4_mapped() {
        let mut one = [0u8; 16];
        one[15] = 1;
        unsafe {
            for from in [in6_addr { s6_addr: [0; 16] }, in6_addr { s6_addr: one }] {
                let (p, l) = raw(&v6(from, 443));
                let Some(Addr::V6(new)) = rewrite(-1, p, l, is_wild_or_loopback, OWN) else {
                    panic!("should be rewritten");
                };
                assert_eq!(v6_as_v4(&new.sin6_addr), Some(OWN));
                assert_eq!(new.sin6_addr.s6_addr[10..12], [0xff, 0xff]);
                assert_eq!(u16::from_be(new.sin6_port), 443);
            }
            let mut global = [0u8; 16];
            global[0] = 0x20;
            global[1] = 0x01;
            let (p, l) = raw(&v6(in6_addr { s6_addr: global }, 443));
            assert!(rewrite(-1, p, l, is_wild_or_loopback, OWN).is_none(), "a real IPv6 address is left alone");
        }
    }

    #[test]
    fn connect_rewrites_only_localhost() {
        unsafe {
            let (p, l) = raw(&v4([127, 0, 0, 1], 5432));
            assert!(matches!(rewrite(-1, p, l, is_loopback, OWN), Some(Addr::V4(a)) if v4_from(a.sin_addr) == OWN));
            for other in [[0, 0, 0, 0], [127, 10, 0, 3], [93, 184, 216, 34]] {
                let (p, l) = raw(&v4(other, 5432));
                assert!(rewrite(-1, p, l, is_loopback, OWN).is_none(), "{other:?}");
            }
        }
    }

    #[test]
    fn addresses_format_like_rust() {
        assert_eq!(fmt_addr(&Addr::V4(v4([127, 10, 0, 2], 80))), "127.10.0.2:80");
        assert_eq!(fmt_addr(&Addr::V6(v6(mapped(OWN), 80))), "[::ffff:127.10.0.2]:80");
    }
}
