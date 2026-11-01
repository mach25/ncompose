//! The supervisor: starts services with the shim injected, captures logs,
//! applies restart policies, runs port forwarders and the proxy, and tears
//! everything down on SIGINT/SIGTERM.

use crate::config::{Plan, RestartPolicy, Service};
use crate::forward;
use crate::state::{Paths, ServiceState, State};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::net::{Ipv4Addr, SocketAddrV4};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, Command};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::{watch, Notify};

const COLORS: [&str; 6] = ["36", "33", "32", "35", "34", "31"];

/// Shared output: prefixed console lines + per-service log files.
#[derive(Clone)]
struct Output {
    console: bool,
    color: bool,
    width: usize,
}

impl Output {
    fn line(&self, name: &str, color_idx: usize, line: &str) {
        if !self.console {
            return;
        }
        let mut out = std::io::stdout().lock();
        let _ = if self.color {
            writeln!(
                out,
                "\x1b[{}m{:<w$} |\x1b[0m {line}",
                COLORS[color_idx % COLORS.len()],
                name,
                w = self.width
            )
        } else {
            writeln!(out, "{:<w$} | {line}", name, w = self.width)
        };
    }
    fn info(&self, msg: &str) {
        // Supervisor messages always go somewhere: console, or the supervisor log
        // (which is our stdout when detached).
        let mut out = std::io::stdout().lock();
        let _ = if self.color {
            writeln!(out, "\x1b[1m{:<w$} |\x1b[0m {msg}", "ncompose", w = self.width)
        } else {
            writeln!(out, "{:<w$} | {msg}", "ncompose", w = self.width)
        };
    }
}

struct Shared {
    paths: Paths,
    state: Mutex<State>,
    out: Output,
    shim: PathBuf,
    shutdown: watch::Receiver<bool>,
    /// Per service: true while it is ready (healthy, or started if it has no healthcheck).
    ready: HashMap<String, watch::Sender<bool>>,
    /// Port of each service's embedded DNS resolver (on the service's own IP).
    dns: HashMap<String, u16>,
}

impl Shared {
    fn update(&self, name: &str, f: impl FnOnce(&mut ServiceState)) {
        let mut st = self.state.lock().unwrap();
        f(st.services.entry(name.to_string()).or_default());
        if let Err(e) = st.save(&self.paths) {
            self.out.info(&format!("warning: cannot save state: {e}"));
        }
    }
}

pub async fn run(plan: Plan, paths: Paths, shim: PathBuf, detached: bool, aliases_added: Vec<Ipv4Addr>) -> Result<()> {
    std::fs::create_dir_all(paths.logs())?;
    let _ = std::fs::remove_file(paths.restart_requests());

    let width = plan.services.iter().map(|s| s.name.len()).max().unwrap_or(0).max(8);
    let out = Output {
        console: !detached,
        color: !detached && std::io::stdout().is_terminal(),
        width,
    };

    let mut state = State {
        project: plan.project.clone(),
        supervisor_pid: std::process::id() as i32,
        detached,
        aliases_added,
        services: Default::default(),
    };
    for s in &plan.services {
        state.services.insert(
            s.name.clone(),
            ServiceState {
                ip: Some(s.ip),
                status: "created".into(),
                ports: s.ports.iter().map(|p| p.to_string()).collect(),
                ..Default::default()
            },
        );
    }
    state.save(&paths)?;

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let ready = plan.services.iter().map(|s| (s.name.clone(), watch::channel(false).0)).collect();

    // Embedded DNS per service (see dns.rs). Without it a service still resolves
    // names through getaddrinfo; only resolvers that bypass libc lose them.
    let mut tasks = tokio::task::JoinSet::new();
    let mut dns = HashMap::new();
    for s in &plan.services {
        match crate::dns::bind(s.ip).await {
            Ok((socket, port)) => {
                dns.insert(s.name.clone(), port);
                tasks.spawn(crate::dns::serve(
                    socket,
                    s.hosts.clone(),
                    crate::dns::upstream_nameservers(),
                    shutdown_rx.clone(),
                ));
            }
            Err(e) => out.info(&format!("{}: no embedded DNS ({e:#})", s.name)),
        }
    }

    let shared = Arc::new(Shared {
        paths: paths.clone(),
        state: Mutex::new(state),
        out: out.clone(),
        shim,
        shutdown: shutdown_rx.clone(),
        ready,
        dns,
    });

    // Bind published ports and the proxy up front so conflicts fail `up` early.
    for s in &plan.services {
        for p in &s.ports {
            let listen = SocketAddrV4::new(p.host_ip, p.host_port);
            let target = SocketAddrV4::new(s.ip, p.port);
            let listener = tokio::net::TcpListener::bind(listen)
                .await
                .with_context(|| format!("service {}: cannot publish {listen}", s.name))?;
            out.info(&format!("publishing {listen} -> {} ({target})", s.name));
            tasks.spawn(forward::port_forwarder(listener, target, shutdown_rx.clone()));
        }
    }
    if let Some(p) = &plan.proxy {
        let listen = SocketAddrV4::new(p.listen_ip, p.listen_port);
        let listener = tokio::net::TcpListener::bind(listen)
            .await
            .with_context(|| format!("proxy: cannot listen on {listen}"))?;
        for (host, target) in &p.routes {
            out.info(&format!("proxy http://{host}:{} -> {target}", p.listen_port));
        }
        tasks.spawn(forward::reverse_proxy(listener, p.routes.clone(), shutdown_rx.clone()));
    }

    // Start services in dependency order.
    let mut restart_signals: HashMap<String, Arc<Notify>> = HashMap::new();
    let mut service_tasks = tokio::task::JoinSet::new();
    for (idx, svc) in plan.services.iter().enumerate() {
        let notify = Arc::new(Notify::new());
        restart_signals.insert(svc.name.clone(), notify.clone());
        service_tasks.spawn(service_loop(shared.clone(), svc.clone(), idx, notify));
    }

    // Wait for a stop signal; SIGUSR1 = restart requests from `ncompose restart`.
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sighup = signal(SignalKind::hangup())?;
    let mut sigusr1 = signal(SignalKind::user_defined1())?;
    loop {
        tokio::select! {
            _ = sigint.recv() => break,
            _ = sigterm.recv() => break,
            _ = sighup.recv() => break,
            _ = sigusr1.recv() => {
                let req = std::fs::read_to_string(paths.restart_requests()).unwrap_or_default();
                let _ = std::fs::remove_file(paths.restart_requests());
                let names: Vec<&str> = req.split_whitespace().collect();
                for (name, n) in &restart_signals {
                    if names.is_empty() || names.contains(&"*") || names.contains(&name.as_str()) {
                        n.notify_one();
                    }
                }
            }
            Some(res) = tasks.join_next() => {
                if let Ok(Err(e)) = res { out.info(&format!("error: {e:#}")); }
            }
        }
    }

    out.info("stopping...");
    let _ = shutdown_tx.send(true);
    // A second Ctrl-C while stopping kills everything immediately.
    let force = async {
        sigint.recv().await;
    };
    tokio::select! {
        _ = async { while service_tasks.join_next().await.is_some() {} } => {}
        _ = force => {
            out.info("forced stop");
            let st = shared.state.lock().unwrap();
            for s in st.services.values() {
                if let Some(pid) = s.pid { unsafe { libc::kill(-pid, libc::SIGKILL); } }
            }
        }
    }
    tasks.abort_all();
    out.info("all services stopped");
    Ok(())
}

async fn service_loop(sh: Arc<Shared>, svc: Service, color: usize, restart: Arc<Notify>) {
    let mut shutdown = sh.shutdown.clone();
    if !wait_for_dependencies(&sh, &svc, &mut shutdown).await {
        sh.update(&svc.name, |s| s.status = "not started".into());
        return;
    }
    let ready = &sh.ready[&svc.name];
    let mut backoff = Duration::from_secs(1);
    let mut first = true;
    loop {
        if *shutdown.borrow() {
            break;
        }
        if !first {
            sh.update(&svc.name, |s| s.restarts += 1);
        }
        first = false;

        let started = Instant::now();
        let mut child = match spawn(&sh, &svc, color) {
            Ok(c) => c,
            Err(e) => {
                sh.out.info(&format!("{}: failed to start: {e:#}", svc.name));
                sh.update(&svc.name, |s| {
                    s.status = "failed to start".into();
                    s.pid = None;
                    s.last_exit = Some(format!("{e:#}"));
                });
                // Treat as a failure for restart purposes.
                if !wait_for_restart(&sh, &svc, false, &mut backoff, &restart, &mut shutdown).await {
                    break;
                }
                continue;
            }
        };
        let pid = child.id().map(|p| p as i32);
        sh.out.info(&format!(
            "{} started (pid {}, ip {})",
            svc.name,
            pid.unwrap_or(-1),
            svc.ip
        ));
        let has_check = svc.healthcheck.is_some();
        sh.update(&svc.name, |s| {
            s.status = if has_check { "starting" } else { "running" }.into();
            s.pid = pid;
        });
        if !has_check {
            ready.send_replace(true);
        }

        let probe = probe_until_healthy(&svc, pid.unwrap_or(0));
        tokio::pin!(probe);
        let mut probing = has_check;
        // Wait for exit / stop / restart, marking the service healthy on the way.
        let restart_now = loop {
            tokio::select! {
                _ = &mut probe, if probing => {
                    probing = false;
                    sh.out.info(&format!("{} is healthy ({:.1}s)", svc.name, started.elapsed().as_secs_f64()));
                    sh.update(&svc.name, |s| s.status = "healthy".into());
                    ready.send_replace(true);
                }
                status = child.wait() => {
                    ready.send_replace(false);
                    let desc = match &status {
                        Ok(st) => describe_exit(st),
                        Err(e) => format!("wait error: {e}"),
                    };
                    let success = status.as_ref().map(|s| s.success()).unwrap_or(false);
                    sh.out.info(&format!("{} exited: {desc}", svc.name));
                    sh.update(&svc.name, |s| {
                        s.status = format!("exited ({desc})");
                        s.pid = None;
                        s.last_exit = Some(desc);
                    });
                    if started.elapsed() > Duration::from_secs(10) {
                        backoff = Duration::from_secs(1);
                    }
                    break wait_for_restart(&sh, &svc, success, &mut backoff, &restart, &mut shutdown).await;
                }
                _ = shutdown.changed() => {
                    ready.send_replace(false);
                    stop_child(&sh, &svc, &mut child).await;
                    break false;
                }
                _ = restart.notified() => {
                    ready.send_replace(false);
                    sh.out.info(&format!("{}: restarting", svc.name));
                    stop_child(&sh, &svc, &mut child).await;
                    backoff = Duration::from_secs(1);
                    break true;
                }
            }
        };
        if !restart_now {
            break;
        }
    }
}

/// Block until every dependency is ready. Returns false if we are shutting down first.
async fn wait_for_dependencies(sh: &Shared, svc: &Service, shutdown: &mut watch::Receiver<bool>) -> bool {
    if svc.depends_on.is_empty() {
        return true;
    }
    sh.update(&svc.name, |s| s.status = format!("waiting for {}", svc.depends_on.join(", ")));
    for dep in &svc.depends_on {
        let mut rx = sh.ready[dep].subscribe();
        tokio::select! {
            r = rx.wait_for(|ready| *ready) => if r.is_err() { return false },
            _ = shutdown.changed() => return false,
        }
    }
    true
}

/// Poll the service's HTTP healthcheck until it answers 2xx from the service itself.
/// Never returns without a healthcheck.
///
/// A 2xx alone proves nothing: until the service binds `ip:port`, a host process
/// listening on the wildcard address at that port (say another project's published
/// port) answers for it. So the service's own process group must also be listening.
async fn probe_until_healthy(svc: &Service, pgid: i32) {
    let Some(hc) = &svc.healthcheck else {
        return std::future::pending().await;
    };
    let interval = Duration::from_millis(hc.interval_ms.unwrap_or(500));
    let addr = SocketAddrV4::new(svc.ip, hc.port);
    loop {
        if let Ok(Ok(true)) = tokio::time::timeout(Duration::from_secs(2), http_ok(addr, &hc.http)).await {
            if group_listens_on(pgid, addr).await {
                return;
            }
        }
        tokio::time::sleep(interval).await;
    }
}

/// Whether a process in group `pgid` has a TCP socket listening on `addr`
/// (or on the wildcard address at its port, which also serves `addr`).
async fn group_listens_on(pgid: i32, addr: SocketAddrV4) -> bool {
    let out = Command::new("/usr/sbin/lsof")
        .args(["-nP", "-a", "-g", &pgid.to_string(), &format!("-iTCP:{}", addr.port()), "-sTCP:LISTEN", "-Fn"])
        .output()
        .await;
    let Ok(out) = out else { return false };
    let port = addr.port();
    let accepted = [
        format!("n{}:{port}", addr.ip()),
        format!("n[::ffff:{}]:{port}", addr.ip()),
        format!("n*:{port}"),
    ];
    String::from_utf8_lossy(&out.stdout).lines().any(|l| accepted.iter().any(|a| a == l))
}

async fn http_ok(addr: SocketAddrV4, path: &str) -> std::io::Result<bool> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(addr).await?;
    s.write_all(format!("GET {path} HTTP/1.0\r\nHost: {addr}\r\n\r\n").as_bytes()).await?;
    // Enough for "HTTP/1.1 200"; a short read may deliver less.
    let mut buf = [0u8; 16];
    let mut n = 0;
    while n < 12 {
        let m = s.read(&mut buf[n..]).await?;
        if m == 0 {
            break;
        }
        n += m;
    }
    let head = String::from_utf8_lossy(&buf[..n]);
    Ok(head.split_whitespace().nth(1).is_some_and(|code| code.starts_with('2')))
}

/// After an exit: decide whether to restart (sleeping with backoff), wait for an
/// explicit restart request, or give up because we are shutting down.
/// Returns true to start the service again.
async fn wait_for_restart(
    sh: &Shared,
    svc: &Service,
    success: bool,
    backoff: &mut Duration,
    restart: &Notify,
    shutdown: &mut watch::Receiver<bool>,
) -> bool {
    if restarts_by_itself(svc.restart, success) {
        let delay = *backoff;
        *backoff = (*backoff * 2).min(Duration::from_secs(30));
        sh.out.info(&format!("{}: restarting in {}s (restart: {:?})", svc.name, delay.as_secs(), svc.restart));
        tokio::select! {
            _ = tokio::time::sleep(delay) => true,
            _ = restart.notified() => true,
            _ = shutdown.changed() => false,
        }
    } else {
        tokio::select! {
            _ = restart.notified() => true,
            _ = shutdown.changed() => false,
        }
    }
}

/// Whether `policy` restarts a service that exited (successfully or not) on its own,
/// without a `restart` request.
fn restarts_by_itself(policy: RestartPolicy, success: bool) -> bool {
    match policy {
        RestartPolicy::No => false,
        RestartPolicy::OnFailure => !success,
        RestartPolicy::Always | RestartPolicy::UnlessStopped => true,
    }
}

fn describe_exit(st: &std::process::ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    match (st.code(), st.signal()) {
        (Some(c), _) => format!("code {c}"),
        (None, Some(sig)) => format!("signal {sig}"),
        _ => "unknown".into(),
    }
}

fn spawn(sh: &Shared, svc: &Service, color: usize) -> Result<Child> {
    let mut cmd = Command::new(&svc.binary);
    cmd.args(&svc.args)
        .current_dir(&svc.workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Own process group so we can signal the service and its children together.
        .process_group(0)
        .kill_on_drop(false);

    // Don't leak our own shim config into services.
    for (k, _) in std::env::vars_os() {
        if let Some(k) = k.to_str() {
            if k.starts_with("NCOMPOSE_") || k == "DYLD_INSERT_LIBRARIES" {
                cmd.env_remove(k);
            }
        }
    }
    cmd.env("HOSTNAME", &svc.name);
    let mut dyld = sh.shim.display().to_string();
    for (k, v) in &svc.env {
        if k == "DYLD_INSERT_LIBRARIES" {
            dyld = format!("{dyld}:{v}");
        } else {
            cmd.env(k, v);
        }
    }
    let hosts = svc
        .hosts
        .iter()
        .map(|(n, ip)| format!("{n}={ip}"))
        .collect::<Vec<_>>()
        .join(",");
    cmd.env("DYLD_INSERT_LIBRARIES", dyld)
        .env("NCOMPOSE_SERVICE", &svc.name)
        .env("NCOMPOSE_IP", svc.ip.to_string())
        .env("NCOMPOSE_HOSTS", hosts);
    if let Some(port) = sh.dns.get(&svc.name) {
        cmd.env("NCOMPOSE_DNS", port.to_string());
    }
    if let Ok(v) = std::env::var("NCOMPOSE_DEBUG") {
        cmd.env("NCOMPOSE_DEBUG", v);
    }

    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning {}", svc.binary.display()))?;

    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(sh.paths.log(&svc.name))?;
    let log = Arc::new(Mutex::new(log));
    if let Some(o) = child.stdout.take() {
        tokio::spawn(pump(o, svc.name.clone(), color, sh.out.clone(), log.clone()));
    }
    if let Some(e) = child.stderr.take() {
        tokio::spawn(pump(e, svc.name.clone(), color, sh.out.clone(), log));
    }
    Ok(child)
}

async fn pump(
    r: impl AsyncRead + Unpin,
    name: String,
    color: usize,
    out: Output,
    log: Arc<Mutex<std::fs::File>>,
) {
    let mut r = BufReader::new(r);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match r.read_until(b'\n', &mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let _ = log.lock().unwrap().write_all(&buf);
                let line = String::from_utf8_lossy(&buf);
                out.line(&name, color, line.trim_end_matches(['\n', '\r']));
            }
        }
    }
}

async fn stop_child(sh: &Shared, svc: &Service, child: &mut Child) {
    let Some(pid) = child.id().map(|p| p as i32) else { return };
    sh.update(&svc.name, |s| s.status = "stopping".into());
    unsafe { libc::kill(-pid, libc::SIGTERM) };
    let status = match tokio::time::timeout(Duration::from_secs(svc.stop_timeout), child.wait()).await {
        Ok(s) => s.ok(),
        Err(_) => {
            sh.out.info(&format!("{}: did not stop in {}s, killing", svc.name, svc.stop_timeout));
            unsafe { libc::kill(-pid, libc::SIGKILL) };
            child.wait().await.ok()
        }
    };
    // Reap stragglers left in the group.
    unsafe { libc::kill(-pid, libc::SIGKILL) };
    let desc = status.map(|s| describe_exit(&s)).unwrap_or_else(|| "unknown".into());
    sh.update(&svc.name, |s| {
        s.status = "stopped".into();
        s.pid = None;
        s.last_exit = Some(desc);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn restart_policies() {
        use RestartPolicy::*;
        for (policy, after_success, after_failure) in
            [(No, false, false), (OnFailure, false, true), (Always, true, true), (UnlessStopped, true, true)]
        {
            assert_eq!(restarts_by_itself(policy, true), after_success, "{policy:?} after success");
            assert_eq!(restarts_by_itself(policy, false), after_failure, "{policy:?} after failure");
        }
    }

    #[test]
    fn exits_are_described_by_code_or_signal() {
        assert_eq!(describe_exit(&std::process::ExitStatus::from_raw(0)), "code 0");
        assert_eq!(describe_exit(&std::process::ExitStatus::from_raw(3 << 8)), "code 3");
        assert_eq!(describe_exit(&std::process::ExitStatus::from_raw(libc::SIGKILL)), "signal 9");
    }

    /// Serves one canned response, written in the given pieces.
    async fn canned(pieces: &'static [&'static [u8]]) -> SocketAddrV4 {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = match l.local_addr().unwrap() {
            std::net::SocketAddr::V4(a) => a,
            _ => unreachable!(),
        };
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let mut buf = [0u8; 256];
                let _ = s.read(&mut buf).await;
                for p in pieces {
                    let _ = s.write_all(p).await;
                    let _ = s.flush().await;
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        });
        addr
    }

    #[tokio::test]
    async fn http_probe_accepts_only_2xx() {
        assert!(http_ok(canned(&[b"HTTP/1.1 200 OK\r\n\r\n"]).await, "/").await.unwrap());
        assert!(http_ok(canned(&[b"HTTP/1.1 204 No Content\r\n\r\n"]).await, "/").await.unwrap());
        assert!(!http_ok(canned(&[b"HTTP/1.1 503 Unavailable\r\n\r\n"]).await, "/").await.unwrap());
        assert!(!http_ok(canned(&[b"garbage"]).await, "/").await.unwrap());
        // A status line that arrives in pieces is still read whole.
        assert!(http_ok(canned(&[b"HTT", b"P/1.1 2", b"00 OK\r\n\r\n"]).await, "/").await.unwrap());
    }

    #[tokio::test]
    async fn listener_ownership_is_checked_by_process_group() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = match l.local_addr().unwrap() {
            std::net::SocketAddr::V4(a) => a,
            _ => unreachable!(),
        };
        let pgid = unsafe { libc::getpgrp() };
        assert!(group_listens_on(pgid, addr).await, "our own listener");
        // A port that was just released (other tests in this process bind ports too).
        let freed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let other = SocketAddrV4::new(*addr.ip(), freed);
        assert!(!group_listens_on(pgid, other).await, "a port nobody in our group listens on");
        // A listener on the same port but another address does not count.
        let elsewhere = SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 2), addr.port());
        assert!(!group_listens_on(pgid, elsewhere).await);
    }
}
