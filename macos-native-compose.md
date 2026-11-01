# Native "Compose" for macOS — Design Notes

A Docker Compose–like tool for running native macOS service binaries on Apple Silicon, **without virtualization and without kernel support for namespaces**. Each service believes it has its own network (it binds `0.0.0.0:80`, talks to `db:5432`, etc.), while the tool fakes that view in userspace.

---

## 1. Goals and non-goals

### Goals
- Start/stop a group of native macOS services from one declarative file (`up` / `down` / `logs` / `ps`).
- **Same-port services**: multiple services can all listen on the same port (e.g. 8080) without config changes.
- **Transparent networking**: services need no special flags, env vars or config to bind to "their" address. The app should not know it is being redirected.
- **Service discovery by name**: service `api` can connect to `db:5432`.
- **Port publishing** like Docker (`ports: "8080:80"` exposes a service on the host).
- **Optional reverse proxy** in front, routing by hostname (e.g. `app.localhost` → `api`).
- **Packaging / reproducible runtime**: a service bundle = binaries + non-Apple dependencies + manifest.

### Non-goals
- Docker compatibility (no Docker API, no OCI runtime spec compliance, no forking dockerd/containerd).
- Running Linux images (that is solved elsewhere by VM-based tools).
- Security isolation. This is **not** a sandbox; a determined process can bypass it.
- Shipping macOS system files or Apple SDKs inside images.

---

## 2. Background: why not "real" containers on macOS

Linux containers are built from kernel features. macOS (XNU) only has partial equivalents:

| Linux feature | macOS equivalent | Gap |
|---|---|---|
| Mount namespace + overlayfs | `chroot`, APFS clones (instant copy-on-write) | chroot needs root; no native overlay FS (would need FSKit or macFUSE) |
| PID namespace | none | Seatbelt can hide processes, not give a private PID 1 |
| Network namespace | none | No per-process network stack → **this project fakes it in userspace** |
| cgroups | `setrlimit`, `taskpolicy`, QoS | No hard memory cap; supervisor must enforce |
| seccomp / LSM | Seatbelt (`sandbox-exec` profiles) | Powerful but deprecated/undocumented |
| User namespaces | real (ephemeral) user accounts | Workable |

Other relevant facts:
- **macOS has no stable syscall ABI** — only `libSystem` is stable. Shipping a full macOS userland in an image breaks across OS versions. → Images contain only the app layer; the host's `/System` and `/usr` are used as-is.
- Prior art for native macOS containers: Darwin Containers (fork of dockerd/containerd/runc, requires SIP disabled, multi-GB images), chroot-based tools (dedock, OSXIEC), full macOS VMs (Tart, Anka, UTM; Apple limits to 2 macOS guests per host).
- Reproducible packaging without isolation already exists: Nix, Flox, Devbox, pkgx, mise. This project's niche is **the networking/compose layer for native binaries**.

---

## 3. Core idea

Docker is a program that asks the Linux kernel to give each process its own network view. Here, the same illusion is produced in **userspace**:

1. Each service gets its own **loopback alias IP** (its "interface").
2. An **interposition shim** (injected with `DYLD_INSERT_LIBRARIES`) rewrites the service's networking calls so it transparently uses that IP.
3. A **supervisor** starts the processes, injects the shim, and manages lifecycle.
4. **Port forwarders** publish selected service ports on the host.
5. An optional **reverse proxy** routes by hostname.

**Why interposition works well on macOS:** Apple does not support raw syscalls, so nearly all binaries (including Go and Rust) go through `libSystem`. That makes libc-level interposition more reliable on macOS than on Linux.

---

## 4. Concept mapping (Compose → macOS)

| Compose concept | Implementation |
|---|---|
| Container network interface | Loopback alias per service: `127.10.0.2`, `127.10.0.3`, … (`ifconfig lo0 alias …`) |
| Network (`backend`, `frontend`) | A loopback subnet per network, e.g. `127.10.0.0/24`, `127.11.0.0/24` |
| Service-name DNS | Resolved **inside the shim** (`getaddrinfo` interposition). Optional fallback: `/etc/hosts` entries or a small DNS server via `/etc/resolver/<domain>` |
| `ports: "8080:80"` | Userspace TCP forwarder on host port → service IP:port (alternative: `pf` rdr rules) |
| Network isolation between groups | Optional `pf` rules blocking traffic between subnets |
| `restart`, `logs`, `up`/`down` | Supervisor process |
| Reverse proxy | Built-in or external (Caddy/Traefik/nginx), host-based routing; `*.localhost` resolves to loopback in browsers |

---

## 5. Example compose file

```yaml
networks:
  backend: { subnet: 127.10.0.0/24 }

services:
  api:
    binary: ./bin/api
    args: []
    networks: [backend]
    ports: ["8080:8080"]          # published to the host
    env:
      DB_URL: postgres://db:5432/app
    restart: on-failure

  db:
    binary: ./bin/postgres
    networks: [backend]            # reachable as "db", not published

  worker:
    binary: ./bin/worker
    networks: [backend]

proxy:
  listen: 80
  routes:
    app.localhost: api:8080
```

`up` sequence: allocate IPs → create loopback aliases → write name table → start services with shim → start port forwarders → start proxy.
`down` reverses everything (stop processes, remove forwarders, remove aliases, clean names).

---

## 6. Components

### 6.1 Interposition shim (the core — build and validate first)
A small C dynamic library loaded via `DYLD_INSERT_LIBRARIES`, using dyld's `__DATA,__interpose` section. Configuration comes from env vars set by the supervisor (e.g. assigned IP, name→IP table, path to a shared table).

Calls to interpose and behaviour:

| Call | Behaviour |
|---|---|
| `bind()` | `0.0.0.0` / `INADDR_ANY` (and `::`) → service's assigned IP. Port unchanged. |
| `connect()` | `127.0.0.1` / `localhost` → service's own IP ("localhost means me", like Docker) |
| `getaddrinfo()` / `gethostbyname()` | Service names (`db`, `api`) → their assigned IPs; everything else passes through |
| `getsockname()` / `getpeername()` | Report the addresses the app expects, hiding rewrites where needed |
| `sendto()` / UDP paths | Same rewriting rules as `connect()` / `bind()` |

Considerations:
- IPv6: decide early (map `::` and `::1` too, or force IPv4).
- Child processes inherit `DYLD_INSERT_LIBRARIES` unless the env is cleared.
- Ports below 1024 can be bound unprivileged on modern macOS.

### 6.2 Supervisor / CLI
- Parses the compose file, allocates IPs, starts/stops/restarts processes, captures logs (per-service, prefixed, like Compose).
- Commands: `up`, `down`, `ps`, `logs [-f] [service]`, `restart [service]`.
- Optional resource controls: `setrlimit`, `taskpolicy`, memory polling + kill on budget.
- Suggested language: Go (CLI, forwarders, proxy). Shim in C.

### 6.3 Privileged helper
Root is needed for: loopback aliases, `/etc/hosts` / `/etc/resolver`, `pf` rules.
- Implement as a minimal launchd daemon that only performs these operations; the main CLI runs as the normal user.
- Simplest prototype: call `sudo` for these steps.

### 6.4 Port forwarder
- Userspace TCP proxy: listen on `0.0.0.0:<host_port>`, forward to `<service_ip>:<port>`. UDP optional.

### 6.5 Reverse proxy (optional)
- Host-header routing to `service:port`. Can be built in (Go `httputil.ReverseProxy`) or generate config for Caddy.

### 6.6 Packaging (later)
- Bundle = service binaries + non-Apple dylibs + manifest.
- Relocation: rewrite Mach-O paths with `install_name_tool` / `@rpath`, then ad hoc re-sign (`codesign -s -`), clear quarantine (`xattr -d com.apple.quarantine`).
- Record minimum macOS version per bundle.
- Optional: distribute as OCI artifacts (`darwin/arm64`) via any registry (e.g. with `oras`); APFS clones for instant per-instance copies.

---

## 7. Constraints and limitations

- **SIP-protected Apple binaries** (`/usr/bin/curl` etc.) ignore `DYLD_INSERT_LIBRARIES`. A service shelling out to them uses the real network.
- **Hardened runtime** blocks DYLD injection → re-sign own binaries ad hoc without hardened runtime.
- Processes that clear their environment before spawning children drop the shim.
- Outbound traffic to the internet uses the host's real network (fine for this use case).
- Not a security boundary.

### Running on your own hardware (not distributing)
- Licensing: using Xcode/SDKs/system files on your own Mac is generally fine; just don't redistribute Apple software (e.g. in public images). (Not legal advice.)
- Technical restrictions can be relaxed by the owner, at a security cost:
  - SIP: `csrutil disable` or `csrutil enable --without …` from Recovery → shim also works on Apple binaries.
  - Network Extensions without special entitlements: `systemextensionsctl developer on` (requires SIP off).
  - Kernel extensions: "Reduced Security" boot policy (deprecated path; large project).
- **The baseline design needs none of these** as long as the services are your own binaries.

### Possible later enforcement layer
A Network Extension (transparent proxy / content filter) can redirect outbound flows per process without app cooperation. It cannot handle `bind()`, so it would complement the shim, not replace it.

---

## 8. Build plan

1. **Shim proof of concept**
   - C dylib interposing `bind()` only.
   - Test: run two copies of a simple HTTP server that both bind `0.0.0.0:8080`, each with a different assigned IP. Both must start; `curl 127.10.0.2:8080` and `curl 127.10.0.3:8080` must hit different instances.
2. **Shim: name resolution and localhost**
   - Interpose `getaddrinfo`, `connect`, `getsockname`. Test service A calling `http://b:8080` and `localhost`.
3. **Minimal runner**
   - Go CLI: parse compose YAML, create aliases (sudo), start services with shim, prefixed logs, `up`/`down`.
4. **Port publishing**
   - TCP forwarders for `ports:` entries.
5. **Reverse proxy**
   - Host-based routing.
6. **Supervisor features**
   - Restart policies, `ps`, `logs -f`, graceful shutdown, cleanup on crash.
7. **Privileged helper**
   - Replace sudo calls with a launchd daemon.
8. **Packaging**
   - Bundle format, relocation, signing, optional OCI distribution.
9. **Optional**: `pf`-based network isolation, Network Extension enforcement, resource limits.

---

## 9. Open questions
- Do target services bind `0.0.0.0`, `127.0.0.1`, or are they configurable? (Determines how much the shim must cover.)
- IPv6 support needed?
- UDP needed, or TCP only?
- Do any services spawn Apple system binaries for networking?
- Should name resolution be shim-only, or also visible to non-shimmed tools via `/etc/hosts` / `/etc/resolver`?
- Language choice confirmed (Go + C)?
