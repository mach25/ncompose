//! ncompose — a Docker Compose–like runner for native macOS services.
//!
//! Each service gets its own loopback alias IP; an injected interposition shim
//! makes `bind(0.0.0.0)` land on that IP, resolves service names, and maps
//! `localhost` to the service itself.

mod config;
mod dns;
mod forward;
mod net;
mod state;
mod supervisor;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use state::{pid_alive, Paths, State};
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(name = "ncompose", version, about = "Compose-style runner for native macOS services")]
struct Cli {
    /// Compose file (default: ncompose.yaml / compose.yaml in the current directory)
    #[arg(short, long)]
    file: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create aliases, start all services, forwarders and proxy
    Up {
        /// Run in the background
        #[arg(short, long)]
        detach: bool,
        /// With -d: wait until every service is healthy (or running, without a healthcheck);
        /// exit non-zero if one exits first or the timeout passes
        #[arg(long, requires = "detach")]
        wait: bool,
        /// Seconds `--wait` waits
        #[arg(long, default_value_t = 120, requires = "wait")]
        timeout: u64,
        /// Remove aliases this run had to create (with sudo) on exit
        #[arg(long)]
        remove_aliases: bool,
    },
    /// Stop everything started by `up`
    Down {
        /// Also remove aliases `up` had to create with sudo
        #[arg(long)]
        remove_aliases: bool,
    },
    /// List services and their status
    Ps,
    /// Show service logs
    Logs {
        /// Follow log output
        #[arg(short, long)]
        follow: bool,
        /// Number of lines to show from the end of each log
        #[arg(short = 'n', long)]
        tail: Option<usize>,
        services: Vec<String>,
    },
    /// Restart services (all if none given)
    Restart { services: Vec<String> },
    /// Validate the compose file and print the resolved plan
    Config,
    /// Manage loopback aliases for this project explicitly (persist across up/down)
    Net {
        #[command(subcommand)]
        action: NetAction,
    },
    /// One-time machine setup so ncompose never needs sudo again
    System {
        #[command(subcommand)]
        action: SystemAction,
    },
    #[command(hide = true, name = "__supervise")]
    Supervise {
        #[arg(long, value_delimiter = ',')]
        aliases_added: Vec<Ipv4Addr>,
    },
}

#[derive(Subcommand)]
enum SystemAction {
    /// Install a LaunchDaemon that creates a pool of loopback aliases at boot (asks for sudo once)
    Install {
        /// Address pools (CIDR inside 127.0.0.0/8); repeat or comma-separate
        #[arg(long = "pool", value_delimiter = ',', default_value = net::DEFAULT_POOL)]
        pools: Vec<String>,
    },
    /// Remove the LaunchDaemon (and, with --remove-aliases, the aliases)
    Uninstall {
        #[arg(long)]
        remove_aliases: bool,
    },
    /// Show whether the daemon is installed and which pool addresses are present
    Status,
}

#[derive(Subcommand)]
enum NetAction {
    /// Add the aliases this project needs (they are then not removed by `down`)
    Up,
    /// Remove this project's aliases
    Down,
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli) {
        eprintln!("ncompose: {e:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    // `system` commands don't need a compose file.
    if let Cmd::System { action } = &cli.cmd {
        return cmd_system(action);
    }
    let file = config::find_compose_file(cli.file.as_deref())?;
    let plan = config::load(&file)?;
    let paths = Paths::new(&plan.dir);

    match cli.cmd {
        Cmd::Config => cmd_config(&plan),
        Cmd::Up { detach, wait, timeout, remove_aliases } => {
            let wait = wait.then(|| Duration::from_secs(timeout));
            cmd_up(plan, paths, &file, detach, wait, !remove_aliases)
        }
        Cmd::Down { remove_aliases } => cmd_down(&paths, !remove_aliases),
        Cmd::System { .. } => unreachable!(),
        Cmd::Ps => cmd_ps(&plan, &paths),
        Cmd::Logs { follow, tail, services } => cmd_logs(&plan, &paths, follow, tail, services),
        Cmd::Restart { services } => cmd_restart(&plan, &paths, services),
        Cmd::Net { action: NetAction::Up } => {
            let added = net::ensure_aliases(&plan.all_ips())?;
            println!("aliases present: {}", fmt_ips(&plan.all_ips()));
            if added.is_empty() {
                println!("(nothing to add)");
            }
            Ok(())
        }
        Cmd::Net { action: NetAction::Down } => {
            if running_supervisor(&paths)?.is_some() {
                bail!("project is running; run `ncompose down` first");
            }
            net::remove_aliases(&plan.all_ips())
        }
        Cmd::Supervise { aliases_added } => {
            let shim = net::shim_path()?;
            tokio_rt()?.block_on(supervisor::run(plan, paths, shim, true, aliases_added))
        }
    }
}

fn cmd_system(action: &SystemAction) -> Result<()> {
    match action {
        SystemAction::Install { pools } => {
            let n: usize = pools.iter().map(|p| net::pool_addrs(p).map(|a| a.len())).sum::<Result<_>>()?;
            net::system_install(pools)?;
            println!("installed: {n} loopback aliases ({}) are created at every boot.", pools.join(", "));
            println!("ncompose up/down no longer need sudo for networks inside these pools.");
            Ok(())
        }
        SystemAction::Uninstall { remove_aliases } => {
            net::system_uninstall(*remove_aliases)?;
            println!("uninstalled");
            Ok(())
        }
        SystemAction::Status => {
            match net::installed_pools() {
                Some(pools) => {
                    let have = net::lo0_addrs()?;
                    for p in pools {
                        let addrs = net::pool_addrs(&p)?;
                        let present = addrs.iter().filter(|a| have.contains(a)).count();
                        println!("pool {p}: {present}/{} aliases present", addrs.len());
                    }
                }
                None => println!("not installed (run `ncompose system install`)"),
            }
            Ok(())
        }
    }
}

fn tokio_rt() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread().enable_all().build()?)
}

fn fmt_ips(ips: &[Ipv4Addr]) -> String {
    ips.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(", ")
}

/// Returns the state if a supervisor for this project is alive.
fn running_supervisor(paths: &Paths) -> Result<Option<State>> {
    Ok(State::load(paths)?.filter(|s| pid_alive(s.supervisor_pid)))
}

fn cmd_config(plan: &config::Plan) -> Result<()> {
    println!("project: {}  (dir {})", plan.project, plan.dir.display());
    for s in &plan.services {
        println!("\n{}:", s.name);
        println!("  ip:        {}", s.ip);
        println!("  networks:  {}", s.networks.join(", "));
        println!("  binary:    {}", s.binary.display());
        if !s.args.is_empty() {
            println!("  args:      {:?}", s.args);
        }
        if !s.ports.is_empty() {
            println!("  ports:     {}", s.ports.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", "));
        }
        println!("  restart:   {:?}", s.restart);
        if !s.depends_on.is_empty() {
            println!("  depends:   {}", s.depends_on.join(", "));
        }
        println!(
            "  resolves:  {}",
            s.hosts.iter().map(|(n, ip)| format!("{n}={ip}")).collect::<Vec<_>>().join(" ")
        );
        for w in net::injection_warnings(&s.binary) {
            println!("  WARNING:   {w}");
        }
    }
    if let Some(p) = &plan.proxy {
        println!("\nproxy on {}:{}", p.listen_ip, p.listen_port);
        for (h, t) in &p.routes {
            println!("  {h} -> {t}");
        }
    }
    Ok(())
}

/// Kill whatever a dead supervisor left behind.
fn cleanup_stale(paths: &Paths, st: &State) {
    for (name, s) in &st.services {
        if let Some(pid) = s.pid {
            if pid_alive(pid) {
                eprintln!("ncompose: killing leftover {name} (pid {pid})");
                unsafe { libc::kill(-pid, libc::SIGKILL) };
            }
        }
    }
    State::remove(paths);
}

fn cmd_up(
    plan: config::Plan,
    paths: Paths,
    file: &Path,
    detach: bool,
    wait: Option<Duration>,
    keep_aliases: bool,
) -> Result<()> {
    if let Some(st) = State::load(&paths)? {
        if pid_alive(st.supervisor_pid) {
            bail!("project {} is already running (supervisor pid {}); use `ncompose down`", st.project, st.supervisor_pid);
        }
        cleanup_stale(&paths, &st);
    }
    let shim = net::shim_path()?;
    for s in &plan.services {
        for w in net::injection_warnings(&s.binary) {
            eprintln!("ncompose: warning: {}: {w}", s.name);
        }
    }

    let added = net::ensure_aliases(&plan.all_ips())?;
    let recorded = if keep_aliases { vec![] } else { added };

    if detach {
        std::fs::create_dir_all(&paths.root)?;
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(paths.supervisor_log())?;
        let exe = std::env::current_exe()?;
        let mut cmd = std::process::Command::new(exe);
        cmd.arg("-f").arg(std::fs::canonicalize(file)?).arg("__supervise");
        if !recorded.is_empty() {
            cmd.arg("--aliases-added").arg(recorded.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(","));
        }
        cmd.stdin(std::process::Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let mut child = cmd.spawn().context("starting background supervisor")?;
        let pid = child.id() as i32;

        // Wait until every service has been started (or, with --wait, is ready),
        // or something died.
        let deadline = Instant::now() + wait.unwrap_or(Duration::from_secs(5));
        loop {
            if let Ok(Some(_)) = child.try_wait() {
                let tail = std::fs::read_to_string(paths.supervisor_log()).unwrap_or_default();
                bail!("supervisor exited during startup:\n{}", last_lines(&tail, 15));
            }
            if let Ok(Some(st)) = State::load(&paths) {
                if st.supervisor_pid == pid {
                    if wait.is_some() {
                        if let Some((name, s)) = st
                            .services
                            .iter()
                            .find(|(_, s)| s.status.starts_with("exited") || s.status.starts_with("failed"))
                        {
                            let log = std::fs::read_to_string(paths.log(name)).unwrap_or_default();
                            bail!("{name} {} before it was ready; last log lines:\n{}", s.status, last_lines(&log, 30));
                        }
                        let ready = |s: &state::ServiceState| s.status == "healthy" || s.status == "running";
                        if st.services.values().all(ready) {
                            break;
                        }
                    } else if st.services.values().all(|s| s.status != "created") {
                        break;
                    }
                }
            }
            if Instant::now() > deadline {
                if wait.is_some() {
                    let st = State::load(&paths)?.unwrap_or_default();
                    let pending: Vec<String> = st
                        .services
                        .iter()
                        .filter(|(_, s)| s.status != "healthy" && s.status != "running")
                        .map(|(n, s)| format!("{n} ({})", s.status))
                        .collect();
                    bail!("timed out waiting for: {}", pending.join(", "));
                }
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        cmd_ps(&plan, &paths)?;
        println!("\nsupervisor pid {pid}; logs: ncompose logs -f   stop: ncompose down");
        return Ok(());
    }

    let res = tokio_rt()?.block_on(supervisor::run(plan, paths.clone(), shim, false, recorded.clone()));
    State::remove(&paths);
    if !recorded.is_empty() {
        if let Err(e) = net::remove_aliases(&recorded) {
            eprintln!("ncompose: could not remove aliases: {e:#}");
        }
    }
    res
}

fn cmd_down(paths: &Paths, keep_aliases: bool) -> Result<()> {
    let Some(st) = State::load(paths)? else {
        println!("nothing running");
        return Ok(());
    };
    let pid = st.supervisor_pid;
    let mut supervisor_cleans_aliases = false;
    if pid_alive(pid) {
        println!("stopping {} (supervisor pid {pid})...", st.project);
        unsafe { libc::kill(pid, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(30);
        while pid_alive(pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
            // A foreground `up` removes its own state file and aliases when done.
            if !st.detached && !paths.state().exists() {
                supervisor_cleans_aliases = true;
                break;
            }
        }
        if pid_alive(pid) && paths.state().exists() {
            eprintln!("supervisor did not exit; killing");
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        if !st.detached {
            supervisor_cleans_aliases = true;
        }
    }
    // Anything still alive (supervisor crashed or was killed)?
    let st_now = State::load(paths)?.unwrap_or(st.clone());
    cleanup_stale(paths, &st_now);
    if !keep_aliases && !supervisor_cleans_aliases && !st.aliases_added.is_empty() {
        net::remove_aliases(&st.aliases_added)?;
    }
    println!("stopped");
    Ok(())
}

fn cmd_ps(plan: &config::Plan, paths: &Paths) -> Result<()> {
    let st = State::load(paths)?;
    let running = st.as_ref().is_some_and(|s| pid_alive(s.supervisor_pid));
    println!("{:<14} {:<12} {:>7} {:<22} {:>8}  PORTS", "SERVICE", "IP", "PID", "STATUS", "RESTARTS");
    for s in &plan.services {
        let ss = st.as_ref().filter(|_| running).and_then(|st| st.services.get(&s.name));
        let (pid, status, restarts) = match ss {
            Some(ss) => {
                let pid = ss.pid.filter(|p| pid_alive(*p));
                (pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into()), ss.status.clone(), ss.restarts)
            }
            None => ("-".into(), "not running".into(), 0),
        };
        let ports = s.ports.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", ");
        println!("{:<14} {:<12} {:>7} {:<22} {:>8}  {ports}", s.name, s.ip.to_string(), pid, status, restarts);
    }
    if let (Some(p), true) = (&plan.proxy, running) {
        for (h, t) in &p.routes {
            println!("proxy: http://{h}{} -> {t}", if p.listen_port == 80 { String::new() } else { format!(":{}", p.listen_port) });
        }
    }
    Ok(())
}

fn last_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

fn cmd_logs(plan: &config::Plan, paths: &Paths, follow: bool, tail: Option<usize>, services: Vec<String>) -> Result<()> {
    let names: Vec<String> = if services.is_empty() {
        plan.services.iter().map(|s| s.name.clone()).collect()
    } else {
        for s in &services {
            if plan.service(s).is_none() {
                bail!("no such service: {s}");
            }
        }
        services
    };
    let width = names.iter().map(|n| n.len()).max().unwrap_or(0);
    let prefix = |n: &str| if names.len() > 1 { format!("{n:<width$} | ") } else { String::new() };

    let mut offsets = Vec::new();
    for n in &names {
        let text = std::fs::read_to_string(paths.log(n)).unwrap_or_default();
        let shown = match tail {
            Some(t) => last_lines(&text, t),
            None => text.trim_end_matches('\n').to_string(),
        };
        let p = prefix(n);
        for l in shown.lines() {
            println!("{p}{l}");
        }
        offsets.push(text.len() as u64);
    }
    if !follow {
        return Ok(());
    }
    let mut partial = vec![String::new(); names.len()];
    loop {
        for (i, n) in names.iter().enumerate() {
            let Ok(mut f) = std::fs::File::open(paths.log(n)) else { continue };
            let len = f.metadata()?.len();
            if len < offsets[i] {
                offsets[i] = 0; // truncated
            }
            if len == offsets[i] {
                continue;
            }
            f.seek(SeekFrom::Start(offsets[i]))?;
            let mut buf = Vec::new();
            f.read_to_end(&mut buf)?;
            offsets[i] += buf.len() as u64;
            partial[i].push_str(&String::from_utf8_lossy(&buf));
            let p = prefix(n);
            while let Some(pos) = partial[i].find('\n') {
                let line: String = partial[i].drain(..=pos).collect();
                println!("{p}{}", line.trim_end_matches(['\n', '\r']));
            }
        }
        std::io::stdout().flush()?;
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn cmd_restart(plan: &config::Plan, paths: &Paths, services: Vec<String>) -> Result<()> {
    let Some(st) = running_supervisor(paths)? else {
        bail!("project is not running");
    };
    for s in &services {
        if plan.service(s).is_none() {
            bail!("no such service: {s}");
        }
    }
    let req = if services.is_empty() { "*".to_string() } else { services.join(" ") };
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.restart_requests())?;
    writeln!(f, "{req}")?;
    unsafe { libc::kill(st.supervisor_pid, libc::SIGUSR1) };
    println!("restart requested: {}", if services.is_empty() { "all services".into() } else { services.join(", ") });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_lines_keeps_the_tail() {
        assert_eq!(last_lines("a\nb\nc\n", 2), "b\nc");
        assert_eq!(last_lines("a\nb", 10), "a\nb");
        assert_eq!(last_lines("", 3), "");
    }

    #[test]
    fn cli_parses_the_documented_commands() {
        let parse = |args: &[&str]| Cli::try_parse_from(std::iter::once("ncompose").chain(args.iter().copied()));
        assert!(matches!(parse(&["up", "-d", "--wait"]).unwrap().cmd, Cmd::Up { detach: true, wait: true, timeout: 120, .. }));
        assert!(parse(&["up", "--wait"]).is_err(), "--wait needs -d");
        assert!(matches!(parse(&["-f", "x.yaml", "logs", "-f", "-n", "5", "api"]).unwrap().cmd, Cmd::Logs { follow: true, tail: Some(5), .. }));
        assert!(matches!(parse(&["system", "install", "--pool", "127.10.0.0/26,127.11.0.0/29"]).unwrap().cmd,
            Cmd::System { action: SystemAction::Install { ref pools } } if pools.len() == 2));
    }
}
