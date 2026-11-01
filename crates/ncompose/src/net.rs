//! Loopback alias management and preflight checks for DYLD injection.
//!
//! Aliases need root. `ncompose system install` asks for sudo once and installs a
//! LaunchDaemon that creates a pool of aliases at every boot, so `up`/`down` never
//! need root afterwards. Without it, `up` falls back to `sudo ifconfig`.

use anyhow::{bail, Context, Result};
use std::collections::HashSet;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::Command;

/// IPv4 addresses currently configured on lo0.
pub fn lo0_addrs() -> Result<HashSet<Ipv4Addr>> {
    let out = Command::new("/sbin/ifconfig").arg("lo0").output().context("running ifconfig")?;
    Ok(parse_ifconfig_inet(&String::from_utf8_lossy(&out.stdout)))
}

/// The IPv4 addresses (`inet` lines) in `ifconfig` output.
fn parse_ifconfig_inet(text: &str) -> HashSet<Ipv4Addr> {
    text.lines()
        .filter_map(|l| l.trim().strip_prefix("inet "))
        .filter_map(|l| l.split_whitespace().next()?.parse().ok())
        .collect()
}

/// Run a shell script as root (directly if we already are, else via sudo).
fn run_privileged(script: &str, show: &str) -> Result<()> {
    let already_root = unsafe { libc::geteuid() } == 0;
    let status = if already_root {
        Command::new("/bin/sh").args(["-c", script]).status()
    } else {
        eprintln!("ncompose: sudo: {show}");
        Command::new("sudo").args(["/bin/sh", "-c", script]).status()
    }
    .context("running sudo")?;
    if !status.success() {
        bail!("privileged command failed ({status})");
    }
    Ok(())
}

fn sudo_ifconfig(args: Vec<Vec<String>>) -> Result<()> {
    if args.is_empty() {
        return Ok(());
    }
    let script = args
        .iter()
        .map(|a| format!("/sbin/ifconfig {}", a.join(" ")))
        .collect::<Vec<_>>()
        .join(" && ");
    run_privileged(&script, &script)
}

// ---------------------------------------------------------------------------
// One-time system install: boot-time alias pool
// ---------------------------------------------------------------------------

pub const DAEMON_LABEL: &str = "dev.ncompose.loopback-aliases";
pub const DEFAULT_POOL: &str = "127.10.0.0/26";

fn daemon_plist_path() -> PathBuf {
    PathBuf::from(format!("/Library/LaunchDaemons/{DAEMON_LABEL}.plist"))
}

/// Usable host addresses of a CIDR pool (.2 .. broadcast-1, matching allocation).
pub fn pool_addrs(cidr: &str) -> Result<Vec<Ipv4Addr>> {
    let (net, prefix) = crate::config::parse_subnet(cidr)?;
    let base = u32::from(net);
    let size = 1u32 << (32 - prefix);
    Ok((2..size - 1).map(|i| Ipv4Addr::from(base + i)).collect())
}

fn plist(pools: &[String]) -> Result<String> {
    let mut cmds = Vec::new();
    for p in pools {
        for ip in pool_addrs(p)? {
            cmds.push(format!("/sbin/ifconfig lo0 alias {ip} up"));
        }
    }
    let script = cmds.join("; ");
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{DAEMON_LABEL}</string>
  <!-- ncompose pools: {pools} -->
  <key>ProgramArguments</key>
  <array><string>/bin/sh</string><string>-c</string><string>{script}</string></array>
  <key>RunAtLoad</key><true/>
</dict>
</plist>
"#,
        pools = pools.join(" ")
    ))
}

pub fn system_install(pools: &[String]) -> Result<()> {
    let content = plist(pools)?;
    let tmp = std::env::temp_dir().join(format!("{DAEMON_LABEL}.{}.plist", std::process::id()));
    std::fs::write(&tmp, content)?;
    let dst = daemon_plist_path();
    let script = format!(
        "launchctl bootout system/{DAEMON_LABEL} 2>/dev/null; \
         install -m 644 -o root -g wheel '{tmp}' '{dst}' && \
         launchctl bootstrap system '{dst}'",
        tmp = tmp.display(),
        dst = dst.display()
    );
    let res = run_privileged(&script, &format!("install {} and load it", dst.display()));
    let _ = std::fs::remove_file(&tmp);
    res?;
    // RunAtLoad runs asynchronously; give it a moment.
    let want: Vec<Ipv4Addr> = pools.iter().map(|p| pool_addrs(p)).collect::<Result<Vec<_>>>()?.concat();
    for _ in 0..50 {
        let have = lo0_addrs()?;
        if want.iter().all(|ip| have.contains(ip)) {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    bail!("daemon installed but aliases did not appear; check `sudo launchctl print system/{DAEMON_LABEL}`")
}

pub fn system_uninstall(remove_aliases_too: bool) -> Result<()> {
    let dst = daemon_plist_path();
    let mut script = format!("launchctl bootout system/{DAEMON_LABEL} 2>/dev/null; rm -f '{}'", dst.display());
    if remove_aliases_too {
        if let Some(pools) = installed_pools() {
            for p in pools {
                for ip in pool_addrs(&p)? {
                    script.push_str(&format!("; /sbin/ifconfig lo0 -alias {ip} 2>/dev/null"));
                }
            }
        }
    }
    run_privileged(&script, &format!("unload and remove {}", dst.display()))
}

/// Pools configured by an installed daemon, if any.
pub fn installed_pools() -> Option<Vec<String>> {
    parse_installed_pools(&std::fs::read_to_string(daemon_plist_path()).ok()?)
}

/// The pools recorded in a plist written by [`plist`].
fn parse_installed_pools(plist_text: &str) -> Option<Vec<String>> {
    let line = plist_text.lines().find_map(|l| l.trim().strip_prefix("<!-- ncompose pools: "))?;
    Some(line.trim_end_matches("-->").split_whitespace().map(String::from).collect())
}

/// Add any missing aliases. Returns the ones that were added.
pub fn ensure_aliases(ips: &[Ipv4Addr]) -> Result<Vec<Ipv4Addr>> {
    let present = lo0_addrs()?;
    let missing: Vec<Ipv4Addr> = ips.iter().copied().filter(|ip| !present.contains(ip)).collect();
    if !missing.is_empty() {
        eprintln!(
            "ncompose: loopback aliases missing for {}; adding them with sudo.\n\
             ncompose: tip: run `ncompose system install` once and this is never needed again.",
            missing.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(", ")
        );
    }
    sudo_ifconfig(
        missing
            .iter()
            .map(|ip| vec!["lo0".into(), "alias".into(), ip.to_string(), "up".into()])
            .collect(),
    )?;
    Ok(missing)
}

pub fn remove_aliases(ips: &[Ipv4Addr]) -> Result<()> {
    let present = lo0_addrs()?;
    sudo_ifconfig(
        ips.iter()
            .filter(|ip| present.contains(ip) && **ip != Ipv4Addr::LOCALHOST)
            .map(|ip| vec!["lo0".into(), "-alias".into(), ip.to_string()])
            .collect(),
    )
}

/// Locate the interposition dylib: $NCOMPOSE_SHIM, else next to this executable.
pub fn shim_path() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("NCOMPOSE_SHIM") {
        return Ok(PathBuf::from(p));
    }
    let exe = std::env::current_exe()?;
    let dir = exe.parent().unwrap_or(Path::new("."));
    for cand in [dir.join("libncompose_shim.dylib"), dir.join("../lib/libncompose_shim.dylib")] {
        if cand.exists() {
            return Ok(cand.canonicalize()?);
        }
    }
    bail!(
        "libncompose_shim.dylib not found next to {} (build the workspace, or set NCOMPOSE_SHIM)",
        exe.display()
    )
}

/// Resolve a binary the way the spawner will (PATH lookup for bare names).
pub fn resolve_binary(bin: &Path) -> Option<PathBuf> {
    if bin.components().count() > 1 {
        return bin.exists().then(|| bin.to_path_buf());
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).map(|d| d.join(bin)).find(|p| p.is_file())
    })
}

/// Warn about binaries that will silently ignore DYLD_INSERT_LIBRARIES.
pub fn injection_warnings(bin: &Path) -> Vec<String> {
    let mut w = Vec::new();
    let Some(path) = resolve_binary(bin) else {
        w.push(format!("{} not found", bin.display()));
        return w;
    };
    let real = path.canonicalize().unwrap_or(path.clone());
    let s = real.to_string_lossy();
    if ["/usr/bin/", "/bin/", "/sbin/", "/usr/sbin/", "/usr/libexec/", "/System/"]
        .iter()
        .any(|p| s.starts_with(p))
    {
        w.push(format!(
            "{s} is SIP-protected; the shim will NOT be loaded (service uses the real network)"
        ));
        return w;
    }
    // Hardened runtime blocks DYLD_INSERT_LIBRARIES unless the binary has the
    // allow-dyld-environment-variables entitlement.
    if let Ok(out) = Command::new("/usr/bin/codesign").args(["-dv", "--entitlements", "-"]).arg(&real).output() {
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let runtime = text
            .lines()
            .any(|l| l.starts_with("CodeDirectory") && l.contains("(runtime)"));
        if runtime && !text.contains("com.apple.security.cs.allow-dyld-environment-variables") {
            w.push(format!(
                "{s} uses the hardened runtime; the shim will be blocked. Re-sign ad hoc: codesign -f -s - {s}"
            ));
        }
    }
    w
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_and_plist_are_valid() {
        let addrs = pool_addrs(DEFAULT_POOL).unwrap();
        assert_eq!(addrs.first().unwrap().to_string(), "127.10.0.2");
        assert_eq!(addrs.last().unwrap().to_string(), "127.10.0.62");
        let path = std::env::temp_dir().join(format!("ncompose-test-{}.plist", std::process::id()));
        std::fs::write(&path, plist(&[DEFAULT_POOL.to_string()]).unwrap()).unwrap();
        let ok = Command::new("/usr/bin/plutil").arg("-lint").arg(&path).status().unwrap().success();
        let _ = std::fs::remove_file(&path);
        assert!(ok, "plutil rejected the generated plist");
    }

    #[test]
    fn ifconfig_inet_lines() {
        let out = "lo0: flags=8049<UP,LOOPBACK,RUNNING,MULTICAST> mtu 16384\n\
                   \tinet 127.0.0.1 netmask 0xff000000\n\
                   \tinet6 ::1 prefixlen 128\n\
                   \tinet 127.10.0.2 netmask 0xff000000\n";
        let addrs = parse_ifconfig_inet(out);
        assert_eq!(addrs.len(), 2);
        assert!(addrs.contains(&Ipv4Addr::new(127, 10, 0, 2)));
    }

    #[test]
    fn installed_pools_round_trip_through_the_plist() {
        let pools = vec!["127.10.0.0/26".to_string(), "127.11.0.0/29".to_string()];
        assert_eq!(parse_installed_pools(&plist(&pools).unwrap()), Some(pools));
        assert_eq!(parse_installed_pools("<plist/>"), None);
    }

    #[test]
    fn pools_skip_network_gateway_and_broadcast() {
        let addrs: Vec<String> = pool_addrs("127.10.0.8/29").unwrap().iter().map(|a| a.to_string()).collect();
        assert_eq!(addrs, ["127.10.0.10", "127.10.0.11", "127.10.0.12", "127.10.0.13", "127.10.0.14"]);
        assert!(pool_addrs("10.0.0.0/24").is_err());
    }

    #[test]
    fn plist_holds_one_alias_command_per_address() {
        let text = plist(&["127.10.0.8/29".to_string()]).unwrap();
        assert_eq!(text.matches("/sbin/ifconfig lo0 alias").count(), 5);
        assert!(text.contains(DAEMON_LABEL));
    }

    #[test]
    fn injection_warnings_name_what_will_not_be_shimmed() {
        let sip = injection_warnings(Path::new("/usr/bin/true"));
        assert!(sip.iter().any(|w| w.contains("SIP-protected")), "{sip:?}");
        let missing = injection_warnings(Path::new("/nonexistent/ncompose-test-binary"));
        assert!(missing.iter().any(|w| w.contains("not found")), "{missing:?}");
        // Our own test binary is ad hoc signed without the hardened runtime.
        let me = std::env::current_exe().unwrap();
        assert!(injection_warnings(&me).is_empty(), "{:?}", injection_warnings(&me));
    }

    #[test]
    fn bare_binary_names_are_looked_up_on_path() {
        assert_eq!(resolve_binary(Path::new("sh")).unwrap().parent().unwrap(), Path::new("/bin"));
        assert!(resolve_binary(Path::new("ncompose-surely-not-on-path")).is_none());
    }
}
