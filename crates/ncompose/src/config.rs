//! Compose file model, validation and address planning.

use anyhow::{anyhow, bail, Context, Result};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

pub const DEFAULT_NETWORK: &str = "default";
pub const DEFAULT_SUBNET: &str = "127.10.0.0/24";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComposeFile {
    pub name: Option<String>,
    #[serde(default)]
    pub networks: IndexMap<String, NetworkDef>,
    pub services: IndexMap<String, ServiceDef>,
    pub proxy: Option<ProxyDef>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkDef {
    pub subnet: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceDef {
    pub binary: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub workdir: Option<String>,
    #[serde(default)]
    pub networks: Vec<String>,
    #[serde(default)]
    pub ports: Vec<String>,
    #[serde(default)]
    pub env: IndexMap<String, EnvValue>,
    #[serde(default)]
    pub restart: RestartPolicy,
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Fixed address for this service (must be inside its first network's subnet).
    pub ip: Option<Ipv4Addr>,
    /// Seconds to wait after SIGTERM before SIGKILL (default 10).
    pub stop_timeout: Option<u64>,
    /// Readiness probe; dependents wait until it passes.
    pub healthcheck: Option<HealthcheckDef>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct HealthcheckDef {
    /// HTTP path probed with GET on the service's own IP; any 2xx is healthy.
    pub http: String,
    pub port: u16,
    /// Milliseconds between probes (default 500).
    pub interval_ms: Option<u64>,
}

/// Allow numbers/bools in `env:` like Compose does.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum EnvValue {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
}
impl std::fmt::Display for EnvValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EnvValue::Str(s) => f.write_str(s),
            EnvValue::Int(i) => write!(f, "{i}"),
            EnvValue::Float(x) => write!(f, "{x}"),
            EnvValue::Bool(b) => write!(f, "{b}"),
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    #[default]
    #[serde(alias = "none", alias = "false")]
    No,
    OnFailure,
    Always,
    UnlessStopped,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyDef {
    /// `80` or `127.0.0.1:80`
    pub listen: serde_yaml::Value,
    #[serde(default)]
    pub routes: IndexMap<String, String>,
}

// ---------------------------------------------------------------------------
// Resolved plan
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortMap {
    pub host_ip: Ipv4Addr,
    pub host_port: u16,
    pub port: u16,
}
impl std::fmt::Display for PortMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}->{}", self.host_ip, self.host_port, self.port)
    }
}

#[derive(Debug, Clone)]
pub struct Service {
    pub name: String,
    pub binary: PathBuf,
    pub args: Vec<String>,
    pub workdir: PathBuf,
    pub env: Vec<(String, String)>,
    pub ip: Ipv4Addr,
    pub networks: Vec<String>,
    pub ports: Vec<PortMap>,
    pub restart: RestartPolicy,
    pub depends_on: Vec<String>,
    pub stop_timeout: u64,
    pub healthcheck: Option<HealthcheckDef>,
    /// name -> ip for every service reachable from this one (shares a network).
    pub hosts: BTreeMap<String, Ipv4Addr>,
}

#[derive(Debug, Clone)]
pub struct Proxy {
    pub listen_ip: Ipv4Addr,
    pub listen_port: u16,
    /// hostname (lowercase) -> target socket address
    pub routes: Vec<(String, std::net::SocketAddrV4)>,
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub project: String,
    pub dir: PathBuf,
    /// Services in start order (dependencies first).
    pub services: Vec<Service>,
    pub proxy: Option<Proxy>,
}

impl Plan {
    pub fn all_ips(&self) -> Vec<Ipv4Addr> {
        self.services.iter().map(|s| s.ip).collect()
    }
    pub fn service(&self, name: &str) -> Option<&Service> {
        self.services.iter().find(|s| s.name == name)
    }
}

pub fn find_compose_file(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p.to_path_buf());
    }
    for name in ["ncompose.yaml", "ncompose.yml", "compose.yaml", "compose.yml"] {
        let p = PathBuf::from(name);
        if p.exists() {
            return Ok(p);
        }
    }
    bail!("no compose file found (looked for ncompose.yaml, compose.yaml); use -f")
}

pub fn load(path: &Path) -> Result<Plan> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let file: ComposeFile =
        serde_yaml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    let path = std::fs::canonicalize(path)?;
    let dir = path.parent().unwrap_or(Path::new("/")).to_path_buf();
    plan(file, dir)
}

pub fn parse_subnet(s: &str) -> Result<(Ipv4Addr, u8)> {
    let (ip, prefix) = s.split_once('/').ok_or_else(|| anyhow!("subnet {s:?} must be CIDR, e.g. 127.10.0.0/24"))?;
    let ip: Ipv4Addr = ip.parse().with_context(|| format!("bad subnet {s:?}"))?;
    let prefix: u8 = prefix.parse().with_context(|| format!("bad subnet {s:?}"))?;
    if !(8..=30).contains(&prefix) {
        bail!("subnet {s:?}: prefix must be between /8 and /30");
    }
    if ip.octets()[0] != 127 {
        bail!("subnet {s:?} must be inside 127.0.0.0/8 (loopback)");
    }
    let mask = u32::MAX << (32 - prefix);
    Ok((Ipv4Addr::from(u32::from(ip) & mask), prefix))
}

fn in_subnet(ip: Ipv4Addr, (net, prefix): (Ipv4Addr, u8)) -> bool {
    let mask = u32::MAX << (32 - prefix);
    u32::from(ip) & mask == u32::from(net)
}

fn parse_port(s: &str, what: &str) -> Result<u16> {
    s.parse().with_context(|| format!("bad {what} {s:?}"))
}

/// `"80"`, `"8080:80"`, `"0.0.0.0:8080:80"` (optional `/tcp` suffix).
///
/// Without an address, a port is published on 127.0.0.1 only. A wildcard (0.0.0.0)
/// publish also answers on that port for every service address whose service is not
/// listening, so it must be asked for explicitly.
pub fn parse_port_map(spec: &str) -> Result<PortMap> {
    let spec = spec.trim();
    let spec = spec.strip_suffix("/tcp").unwrap_or(spec);
    if spec.ends_with("/udp") {
        bail!("port {spec:?}: UDP publishing is not supported yet");
    }
    let parts: Vec<&str> = spec.split(':').collect();
    Ok(match parts.as_slice() {
        [p] => {
            let p = parse_port(p, "port")?;
            PortMap { host_ip: Ipv4Addr::LOCALHOST, host_port: p, port: p }
        }
        [h, p] => PortMap {
            host_ip: Ipv4Addr::LOCALHOST,
            host_port: parse_port(h, "host port")?,
            port: parse_port(p, "port")?,
        },
        [ip, h, p] => PortMap {
            host_ip: ip.parse().with_context(|| format!("bad host ip in {spec:?}"))?,
            host_port: parse_port(h, "host port")?,
            port: parse_port(p, "port")?,
        },
        _ => bail!("bad port mapping {spec:?}"),
    })
}

fn plan(file: ComposeFile, dir: PathBuf) -> Result<Plan> {
    let project = file
        .name
        .clone()
        .or_else(|| dir.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "default".into());

    if file.services.is_empty() {
        bail!("no services defined");
    }

    // Networks (implicit "default" if a service names none).
    let mut networks: IndexMap<String, (Ipv4Addr, u8)> = IndexMap::new();
    for (name, n) in &file.networks {
        networks.insert(name.clone(), parse_subnet(&n.subnet).with_context(|| format!("network {name}"))?);
    }
    let needs_default = file.services.values().any(|s| s.networks.is_empty());
    if needs_default && !networks.contains_key(DEFAULT_NETWORK) {
        networks.insert(DEFAULT_NETWORK.into(), parse_subnet(DEFAULT_SUBNET)?);
    }
    // Overlapping subnets would make isolation and allocation ambiguous.
    let nets: Vec<_> = networks.iter().collect();
    for (i, (a, sa)) in nets.iter().enumerate() {
        for (b, sb) in &nets[i + 1..] {
            if in_subnet(sa.0, **sb) || in_subnet(sb.0, **sa) {
                bail!("networks {a} and {b} have overlapping subnets");
            }
        }
    }

    // Validate service names (they become hostnames).
    for name in file.services.keys() {
        if name.is_empty()
            || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        {
            bail!("service name {name:?} must be alphanumeric (plus - _ .)");
        }
    }

    // Allocate IPs. Fixed IPs first, then sequential from .2 in each subnet.
    let mut used: HashSet<Ipv4Addr> = HashSet::new();
    let mut ips: IndexMap<String, Ipv4Addr> = IndexMap::new();
    let primary_net = |s: &ServiceDef| -> String {
        s.networks.first().cloned().unwrap_or_else(|| DEFAULT_NETWORK.to_string())
    };
    for (name, s) in &file.services {
        for n in &s.networks {
            if !networks.contains_key(n) {
                bail!("service {name}: unknown network {n:?}");
            }
        }
        if let Some(ip) = s.ip {
            let net = networks[&primary_net(s)];
            if !in_subnet(ip, net) {
                bail!("service {name}: ip {ip} is not in network {}", primary_net(s));
            }
            if !used.insert(ip) {
                bail!("service {name}: ip {ip} already used");
            }
            ips.insert(name.clone(), ip);
        }
    }
    for (name, s) in &file.services {
        if ips.contains_key(name) {
            continue;
        }
        let (net, prefix) = networks[&primary_net(s)];
        let ip = host_addrs(net, prefix)
            .find(|ip| !used.contains(ip))
            .ok_or_else(|| anyhow!("network {} is full", primary_net(s)))?;
        used.insert(ip);
        ips.insert(name.clone(), ip);
    }

    // Build services.
    let svc_networks = |s: &ServiceDef| -> Vec<String> {
        if s.networks.is_empty() {
            vec![DEFAULT_NETWORK.to_string()]
        } else {
            s.networks.clone()
        }
    };
    let mut services = Vec::new();
    for (name, s) in &file.services {
        let my_nets = svc_networks(s);
        let mut hosts = BTreeMap::new();
        for (other, os) in &file.services {
            if svc_networks(os).iter().any(|n| my_nets.contains(n)) {
                hosts.insert(other.to_ascii_lowercase(), ips[other]);
            }
        }
        for d in &s.depends_on {
            if !file.services.contains_key(d) {
                bail!("service {name}: depends_on unknown service {d:?}");
            }
        }
        if let Some(hc) = &s.healthcheck {
            if !hc.http.starts_with('/') {
                bail!("service {name}: healthcheck.http must be a path starting with '/'");
            }
        }
        let binary = if s.binary.contains('/') {
            let p = dir.join(expand_home(&s.binary));
            std::fs::canonicalize(&p).unwrap_or(p)
        } else {
            PathBuf::from(&s.binary) // looked up on PATH at spawn time
        };
        let ports = s
            .ports
            .iter()
            .map(|p| parse_port_map(p))
            .collect::<Result<Vec<_>>>()
            .with_context(|| format!("service {name}"))?;
        services.push(Service {
            name: name.clone(),
            binary,
            args: s.args.clone(),
            workdir: s.workdir.as_ref().map(|w| dir.join(expand_home(w))).unwrap_or_else(|| dir.clone()),
            env: s.env.iter().map(|(k, v)| (k.clone(), v.to_string())).collect(),
            ip: ips[name],
            networks: my_nets,
            ports,
            restart: s.restart,
            depends_on: s.depends_on.clone(),
            stop_timeout: s.stop_timeout.unwrap_or(10),
            healthcheck: s.healthcheck.clone(),
            hosts,
        });
    }

    // Published addresses must not collide (the same port on different addresses is fine,
    // but a wildcard address takes the port on every address).
    let mut published: Vec<std::net::SocketAddrV4> = Vec::new();
    for s in &services {
        for p in &s.ports {
            let addr = std::net::SocketAddrV4::new(p.host_ip, p.host_port);
            if let Some(other) = published.iter().find(|o| addrs_collide(o, &addr)) {
                bail!("service {}: {addr} is already published ({other})", s.name);
            }
            published.push(addr);
        }
    }

    let services = topo_sort(services)?;

    let proxy = match file.proxy {
        None => None,
        Some(p) => {
            let listen = match &p.listen {
                serde_yaml::Value::Number(n) => format!("127.0.0.1:{n}"),
                serde_yaml::Value::String(s) if !s.contains(':') => format!("127.0.0.1:{s}"),
                serde_yaml::Value::String(s) => s.clone(),
                other => bail!("proxy.listen: unexpected value {other:?}"),
            };
            let listen: std::net::SocketAddrV4 =
                listen.parse().with_context(|| format!("proxy.listen {listen:?}"))?;
            if let Some(other) = published.iter().find(|o| addrs_collide(o, &listen)) {
                bail!("proxy {listen} collides with a published port ({other})");
            }
            let mut routes = Vec::new();
            for (host, target) in &p.routes {
                let (svc, port) = target
                    .rsplit_once(':')
                    .ok_or_else(|| anyhow!("proxy route {host}: target {target:?} must be service:port"))?;
                let port = parse_port(port, "port")?;
                let ip = match services.iter().find(|s| s.name == svc) {
                    Some(s) => s.ip,
                    None => svc
                        .parse()
                        .map_err(|_| anyhow!("proxy route {host}: unknown service {svc:?}"))?,
                };
                routes.push((host.to_ascii_lowercase(), std::net::SocketAddrV4::new(ip, port)));
            }
            Some(Proxy { listen_ip: *listen.ip(), listen_port: listen.port(), routes })
        }
    };

    Ok(Plan { project, dir, services, proxy })
}

/// Whether two listeners would fight over the same address and port.
fn addrs_collide(a: &std::net::SocketAddrV4, b: &std::net::SocketAddrV4) -> bool {
    a.port() == b.port() && (a.ip() == b.ip() || a.ip().is_unspecified() || b.ip().is_unspecified())
}

/// The addresses of a subnet that services get: .2 up to the one before broadcast
/// (.1 is left for the gateway role, as in Docker). The boot-time alias pool uses the
/// same range, so every address `up` allocates inside it already exists.
pub fn host_addrs(net: Ipv4Addr, prefix: u8) -> impl Iterator<Item = Ipv4Addr> {
    let base = u32::from(net);
    let size = 1u32 << (32 - u32::from(prefix));
    (2..size.saturating_sub(1)).map(move |i| Ipv4Addr::from(base + i))
}

/// `~/x` -> `$HOME/x`
fn expand_home(p: &str) -> PathBuf {
    match (p.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(p),
    }
}

fn topo_sort(services: Vec<Service>) -> Result<Vec<Service>> {
    let mut out: Vec<Service> = Vec::new();
    let mut remaining = services;
    while !remaining.is_empty() {
        let ready = remaining
            .iter()
            .position(|s| s.depends_on.iter().all(|d| out.iter().any(|o| &o.name == d)))
            .ok_or_else(|| {
                anyhow!(
                    "dependency cycle among: {}",
                    remaining.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(", ")
                )
            })?;
        out.push(remaining.remove(ready));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(yaml: &str) -> Result<Plan> {
        plan(serde_yaml::from_str(yaml)?, PathBuf::from("/proj"))
    }

    #[test]
    fn example_from_design_doc() {
        let plan = p(r#"
networks:
  backend: { subnet: 127.10.0.0/24 }
services:
  api:
    binary: ./bin/api
    networks: [backend]
    ports: ["8080:8080"]
    env: { DB_URL: postgres://db:5432/app }
    restart: on-failure
    depends_on: [db]
  db:
    binary: ./bin/postgres
    networks: [backend]
  worker:
    binary: ./bin/worker
    networks: [backend]
proxy:
  listen: 80
  routes:
    app.localhost: api:8080
"#)
        .unwrap();
        let names: Vec<_> = plan.services.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["db", "api", "worker"]);
        assert_eq!(plan.service("api").unwrap().ip, Ipv4Addr::new(127, 10, 0, 2));
        assert_eq!(plan.service("db").unwrap().ip, Ipv4Addr::new(127, 10, 0, 3));
        assert_eq!(plan.service("api").unwrap().binary, PathBuf::from("/proj/./bin/api"));
        let proxy = plan.proxy.unwrap();
        assert_eq!(proxy.listen_port, 80);
        assert_eq!(proxy.routes[0].1.to_string(), "127.10.0.2:8080");
    }

    #[test]
    fn network_isolation_in_hosts_table() {
        let plan = p(r#"
networks:
  a: { subnet: 127.10.0.0/24 }
  b: { subnet: 127.11.0.0/24 }
services:
  x: { binary: x, networks: [a] }
  y: { binary: y, networks: [a, b] }
  z: { binary: z, networks: [b] }
"#)
        .unwrap();
        let x = plan.service("x").unwrap();
        assert!(x.hosts.contains_key("y") && !x.hosts.contains_key("z"));
        assert_eq!(plan.service("z").unwrap().ip, Ipv4Addr::new(127, 11, 0, 2));
        assert_eq!(plan.service("y").unwrap().hosts.len(), 3);
    }

    #[test]
    fn port_maps() {
        assert_eq!(parse_port_map("80").unwrap().to_string(), "127.0.0.1:80->80");
        assert_eq!(parse_port_map("8080:80").unwrap().to_string(), "127.0.0.1:8080->80");
        assert_eq!(parse_port_map("0.0.0.0:8080:80/tcp").unwrap().to_string(), "0.0.0.0:8080->80");
        assert!(parse_port_map("53:53/udp").is_err());
    }

    fn err(yaml: &str) -> String {
        format!("{:#}", p(yaml).expect_err("expected the file to be rejected"))
    }

    #[test]
    fn services_without_networks_share_the_default_network() {
        let plan = p("services: { a: { binary: a }, b: { binary: b } }").unwrap();
        let a = plan.service("a").unwrap();
        assert_eq!(a.networks, [DEFAULT_NETWORK]);
        assert_eq!(a.ip, Ipv4Addr::new(127, 10, 0, 2));
        assert_eq!(plan.service("b").unwrap().ip, Ipv4Addr::new(127, 10, 0, 3));
        assert_eq!(a.hosts.len(), 2);
    }

    #[test]
    fn fixed_ips_are_reserved_before_allocation() {
        let plan = p(r#"
networks: { n: { subnet: 127.10.0.0/29 } }
services:
  a: { binary: a, networks: [n] }
  b: { binary: b, networks: [n], ip: 127.10.0.2 }
"#)
        .unwrap();
        assert_eq!(plan.service("b").unwrap().ip, Ipv4Addr::new(127, 10, 0, 2));
        assert_eq!(plan.service("a").unwrap().ip, Ipv4Addr::new(127, 10, 0, 3));
    }

    #[test]
    fn subnets_are_normalised_and_validated() {
        // Host bits are dropped: 127.10.0.9/29 is the 127.10.0.8/29 network.
        let plan = p("networks: { n: { subnet: 127.10.0.9/29 } }\nservices: { a: { binary: a, networks: [n] } }")
            .unwrap();
        assert_eq!(plan.service("a").unwrap().ip, Ipv4Addr::new(127, 10, 0, 10));
        assert!(err("networks: { n: { subnet: 127.10.0.0 } }\nservices: { a: { binary: a, networks: [n] } }")
            .contains("CIDR"));
        assert!(err("networks: { n: { subnet: 127.10.0.0/31 } }\nservices: { a: { binary: a, networks: [n] } }")
            .contains("prefix"));
        assert!(err("networks: { n: { subnet: 10.0.0.0/24 } }\nservices: { a: { binary: a, networks: [n] } }")
            .contains("127.0.0.0/8"));
    }

    #[test]
    fn overlapping_networks_are_rejected() {
        let e = err(r#"
networks: { a: { subnet: 127.10.0.0/24 }, b: { subnet: 127.10.0.128/25 } }
services: { x: { binary: x, networks: [a] } }
"#);
        assert!(e.contains("overlapping"), "{e}");
    }

    #[test]
    fn a_full_network_is_an_error() {
        // A /30 has room for exactly one service (.2).
        let e = err(r#"
networks: { n: { subnet: 127.10.0.0/30 } }
services: { a: { binary: a, networks: [n] }, b: { binary: b, networks: [n] } }
"#);
        assert!(e.contains("full"), "{e}");
    }

    #[test]
    fn fixed_ip_must_be_free_and_inside_the_network() {
        assert!(err("services: { a: { binary: a, ip: 127.11.0.2 } }").contains("not in network"));
        assert!(err("services: { a: { binary: a, ip: 127.10.0.2 }, b: { binary: b, ip: 127.10.0.2 } }")
            .contains("already used"));
    }

    #[test]
    fn service_names_must_be_hostnames() {
        assert!(err("services: { \"a b\": { binary: a } }").contains("alphanumeric"));
        assert!(p("services: { db-1.internal_x: { binary: a } }").is_ok());
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(err("services: { a: { binary: a, image: postgres } }").contains("image"));
        assert!(p("services: {}\nvolumes: {}").is_err());
    }

    #[test]
    fn host_ports_and_the_proxy_port_must_be_unique() {
        assert!(err(r#"services: { a: { binary: a, ports: ["80"] }, b: { binary: b, ports: ["8080:80", "80:81"] } }"#)
            .contains("already published"));
        assert!(err("services: { a: { binary: a, ports: [\"80\"] } }\nproxy: { listen: 80 }")
            .contains("collides"));
        // A wildcard address takes the port everywhere.
        assert!(err(r#"services: { a: { binary: a, ports: ["0.0.0.0:80:80"] }, b: { binary: b, ports: ["127.0.0.2:80:80"] } }"#)
            .contains("already published"));
        assert!(err("services: { a: { binary: a, ports: [\"0.0.0.0:80:80\"] } }\nproxy: { listen: 80 }")
            .contains("collides"));
    }

    #[test]
    fn the_same_port_on_different_addresses_is_fine() {
        let plan = p(r#"
services:
  a: { binary: a, ports: ["127.0.0.1:80:80"] }
  b: { binary: b, ports: ["127.0.0.2:80:80"] }
proxy: { listen: "127.0.0.3:80" }
"#)
        .unwrap();
        assert_eq!(plan.services.len(), 2);
        assert_eq!(plan.proxy.unwrap().listen_port, 80);
    }

    #[test]
    fn allocation_and_the_alias_pool_use_the_same_addresses() {
        let pool = crate::net::pool_addrs("127.10.0.0/29").unwrap();
        assert_eq!(pool.first(), Some(&Ipv4Addr::new(127, 10, 0, 2)));
        assert_eq!(pool.last(), Some(&Ipv4Addr::new(127, 10, 0, 6)));
        let yaml = "networks: { n: { subnet: 127.10.0.0/29 } }\nservices:\n"
            .to_string()
            + &(0..pool.len()).map(|i| format!("  s{i}: {{ binary: x, networks: [n] }}\n")).collect::<String>();
        let ips: Vec<Ipv4Addr> = p(&yaml).unwrap().services.iter().map(|s| s.ip).collect();
        assert_eq!(ips, pool);
    }

    #[test]
    fn port_specs_that_are_not_ports_are_rejected() {
        assert!(parse_port_map("http").is_err());
        assert!(parse_port_map("70000").is_err());
        assert!(parse_port_map("1:2:3:4").is_err());
        assert!(parse_port_map("nope:80:80").is_err());
    }

    #[test]
    fn proxy_listen_forms_default_to_localhost() {
        let listen = |v: &str| {
            let plan = p(&format!("services: {{ a: {{ binary: a }} }}\nproxy: {{ listen: {v} }}")).unwrap();
            let px = plan.proxy.unwrap();
            format!("{}:{}", px.listen_ip, px.listen_port)
        };
        assert_eq!(listen("80"), "127.0.0.1:80");
        assert_eq!(listen("\"8088\""), "127.0.0.1:8088");
        assert_eq!(listen("\"0.0.0.0:80\""), "0.0.0.0:80");
    }

    #[test]
    fn proxy_routes_name_services_or_addresses() {
        let plan = p(r#"
services: { api: { binary: a } }
proxy: { listen: 80, routes: { App.Localhost: "api:8080", other.localhost: "127.0.0.9:81" } }
"#)
        .unwrap();
        let routes = plan.proxy.unwrap().routes;
        assert_eq!(routes[0].0, "app.localhost");
        assert_eq!(routes[0].1.to_string(), "127.10.0.2:8080");
        assert_eq!(routes[1].1.to_string(), "127.0.0.9:81");
        assert!(err("services: { api: { binary: a } }\nproxy: { listen: 80, routes: { x.localhost: \"web:80\" } }")
            .contains("unknown service"));
        assert!(err("services: { api: { binary: a } }\nproxy: { listen: 80, routes: { x.localhost: api } }")
            .contains("service:port"));
    }

    #[test]
    fn env_values_restart_policies_and_healthchecks() {
        let plan = p(r#"
services:
  a:
    binary: a
    env: { S: text, I: 5, F: 1.5, B: true }
    restart: unless-stopped
    healthcheck: { http: /health, port: 8080 }
  b: { binary: b, restart: "no" }
  c: { binary: c, restart: always }
"#)
        .unwrap();
        let a = plan.service("a").unwrap();
        assert_eq!(a.env, [("S", "text"), ("I", "5"), ("F", "1.5"), ("B", "true")].map(|(k, v)| (k.to_string(), v.to_string())));
        assert_eq!(a.restart, RestartPolicy::UnlessStopped);
        assert_eq!(a.healthcheck.as_ref().unwrap().http, "/health");
        assert_eq!(plan.service("b").unwrap().restart, RestartPolicy::No);
        assert_eq!(plan.service("c").unwrap().restart, RestartPolicy::Always);
        assert!(err("services: { a: { binary: a, healthcheck: { http: health, port: 1 } } }").contains("'/'"));
    }

    #[test]
    fn dependencies_start_first_whatever_the_file_order() {
        let plan = p(r#"
services:
  web: { binary: w, depends_on: [api] }
  api: { binary: a, depends_on: [db, cache] }
  cache: { binary: c }
  db: { binary: d }
"#)
        .unwrap();
        let order: Vec<_> = plan.services.iter().map(|s| s.name.as_str()).collect();
        let pos = |n| order.iter().position(|x| *x == n).unwrap();
        assert!(pos("db") < pos("api") && pos("cache") < pos("api") && pos("api") < pos("web"));
        assert!(err("services: { a: { binary: a, depends_on: [a] } }").contains("cycle"));
    }

    #[test]
    fn binaries_resolve_against_the_project_directory_or_home() {
        let plan = p("services: { a: { binary: ./bin/a }, b: { binary: postgres }, c: { binary: ~/bin/c } }").unwrap();
        assert_eq!(plan.service("a").unwrap().binary, PathBuf::from("/proj/./bin/a"));
        assert_eq!(plan.service("b").unwrap().binary, PathBuf::from("postgres"));
        let home = std::env::var("HOME").unwrap();
        assert_eq!(plan.service("c").unwrap().binary, PathBuf::from(home).join("bin/c"));
        assert_eq!(plan.service("a").unwrap().workdir, PathBuf::from("/proj"));
    }

    #[test]
    fn rejects_bad_input() {
        assert!(p("services: { a: { binary: a, networks: [nope] } }").is_err());
        assert!(p("networks: { n: { subnet: 10.0.0.0/24 } }\nservices: { a: { binary: a, networks: [n] } }").is_err());
        assert!(p("services: { a: { binary: a, depends_on: [b] }, b: { binary: b, depends_on: [a] } }").is_err());
    }
}
