# ncompose

**Docker Compose for native macOS binaries.** No VM, no containers, no Linux. Just your own
binaries, each believing it has its own network.

```yaml
services:
  db:
    binary: postgres
    args: ["-D", ".data/pg"]
  api:
    binary: ./bin/api            # binds 0.0.0.0:8080 and talks to db:5432; no code changes
    ports: ["8080:8080"]
    depends_on: [db]
  worker:
    binary: ./bin/api            # a second copy, also on port 8080, no conflict
```

```console
$ ncompose up -d --wait
SERVICE   IP           PID    STATUS    RESTARTS  PORTS
db        127.10.0.2   41022  running          0
api       127.10.0.3   41023  running          0  127.0.0.1:8080->8080
worker    127.10.0.4   41024  running          0
```

On Apple Silicon, Linux containers run inside a VM. When what you actually ship is a native macOS
binary (a GraalVM native image, a Go or Rust server, Homebrew's PostgreSQL), that VM only gets in
the way. ncompose gives each service its own loopback address and fakes the rest in userspace:

- **Same port, many services.** Every service can bind `0.0.0.0:8080`.
- **Service discovery by name.** `db:5432` resolves to the `db` service.
- **`localhost` means "me"**, as in a container.
- **Port publishing** (`ports: ["8080:80"]`) and a **host-routing HTTP proxy** (`app.localhost` → `api`).
- **Compose-style lifecycle:** `up`/`down`/`ps`/`logs -f`/`restart`, restart policies,
  healthchecks, and dependency ordering that waits for health.

It is a **development and test tool, not a sandbox.** Processes share the filesystem and can
step around the illusion if they try (see [Limits](#limits)).

## How it works

```
                ┌────────────── ncompose supervisor ─────────────────┐
                │ allocates IPs · starts/stops · restart policies    │
                │ healthchecks · logs · port forwarders · proxy      │
                │ embedded DNS (one resolver per service)            │
                └──────┬──────────────────┬──────────────────┬───────┘
                       │ spawns with DYLD_INSERT_LIBRARIES=libncompose_shim.dylib
                ┌──────▼──────┐    ┌──────▼──────┐    ┌──────▼──────┐
                │ db          │    │ api         │    │ worker      │
                │ 127.10.0.2  │◄───│ 127.10.0.3  │    │ 127.10.0.4  │
                └─────────────┘    └─────────────┘    └─────────────┘
                     loopback aliases on lo0, one per service
```

macOS has no network namespaces, but almost every binary reaches the network through
`libSystem`. Apple supports no raw syscall ABI, so Go and Rust binaries go through `libSystem` too.
ncompose injects a small interposition library into each service and rewrites the calls that matter:

| Call | Rewrite |
|---|---|
| `bind` | `0.0.0.0`, `127.0.0.1`, `::`, `::1` → the service's own IP (port kept). Dual-stack IPv6 sockets get `::ffff:<ip>`; explicitly v6-only sockets get `EADDRNOTAVAIL`, as on a host without IPv6. A wildcard bind to port 0 (a client socket) is left alone. |
| `connect`, `sendto`, `sendmsg` | `127.0.0.1` / `::1` → the service's own IP. Sockets talking to `127/8` get the service's IP as source, so peers see who is calling. |
| `getaddrinfo`, `gethostbyname` | Names of services sharing a network → their IPs. Everything else passes through. |
| UDP to port 53 | Redirected to the service's embedded resolver, for resolvers that bypass libc (Netty, c-ares, …). It answers service names and forwards everything else to the host's nameservers, like Docker's `127.0.0.11`. `recvfrom`/`recvmsg` report each reply as coming from the nameserver its query asked (matched by DNS transaction ID). |
| `getpeername` | The service's own IP → `127.0.0.1`, hiding the `localhost` rewrite. |
| `close` | Forgets the socket's DNS bookkeeping, since the descriptor number gets reused. |

The design notes are in [`macos-native-compose.md`](macos-native-compose.md).

## Requirements

- macOS on Apple Silicon. Intel Macs are untested but nothing is arm64-specific.
- A Rust toolchain (`rustup`), to build.
- Administrator rights **once**, to install the loopback alias pool.

## Install

```sh
git clone <this repository> ncompose && cd ncompose
cargo build --release
target/release/ncompose system install
```

`system install` asks for `sudo` once. It installs a LaunchDaemon that creates the loopback aliases
`127.10.0.2`–`127.10.0.62` at every boot, then exits. Nothing keeps running and nothing listens.
After that, `up` and `down` never need root.

- `--pool 127.10.0.0/24,127.11.0.0/24` reserves more addresses.
- `system status` shows what is present; `system uninstall --remove-aliases` removes it all.
- Without the daemon, `up` falls back to adding missing aliases with `sudo`.

Keep `ncompose` and `libncompose_shim.dylib` together; the CLI finds the library next to itself.
Set `NCOMPOSE_SHIM` to point elsewhere.

## The compose file

`ncompose.yaml` (or `compose.yaml`) in the current directory, or `-f <file>`:

```yaml
name: shop                           # default: the directory name

networks:                            # optional; services without networks share "default" (127.10.0.0/24)
  backend:  { subnet: 127.10.0.0/26 }
  frontend: { subnet: 127.11.0.0/26 }  # outside the default pool: `system install --pool …` it

services:
  db:
    binary: postgres                 # PATH lookup, or a path relative to this file, or ~/…
    args: ["-D", ".data/pg"]
    workdir: .                       # default: the compose file's directory
    networks: [backend]
    ip: 127.10.0.2                   # optional fixed address
    stop_timeout: 20                 # seconds between SIGTERM and SIGKILL (default 10)

  api:
    binary: ./bin/api
    networks: [backend, frontend]    # resolves names on both; its IP comes from the first
    env:
      DATABASE_URL: postgres://db:5432/shop
      WORKERS: 4
    ports:
      - "8080:8080"                  # published on 127.0.0.1
      - "0.0.0.0:9090:9090"          # an explicit address publishes on the LAN
    healthcheck: { http: /health, port: 8080, interval_ms: 500 }
    depends_on: [db]                 # starts once db is ready (healthy, if it has a healthcheck)
    restart: on-failure              # no | on-failure | always | unless-stopped

proxy:                               # optional host-header routing, HTTP/1.x and WebSockets
  listen: 80                         # 127.0.0.1:80; *.localhost resolves to loopback in browsers
  routes:
    shop.localhost: api:8080
    "*.preview.localhost": api:8080
```

Services see the variables you give them plus the host's environment, with `HOSTNAME` set to the
service name.

## Commands

| Command | |
|---|---|
| `ncompose up` | Start everything in the foreground with colour-prefixed logs; Ctrl-C stops (twice: kill). |
| `ncompose up -d [--wait [--timeout S]]` | Start in the background. With `--wait`, return once every service is healthy (or running, without a healthcheck); exit non-zero if one dies first. |
| `ncompose down [--remove-aliases]` | Stop the project. Aliases `up` added with sudo are removed with `--remove-aliases` here or on `up`. |
| `ncompose ps` | Services, IPs, PIDs, status, restarts, ports. |
| `ncompose logs [-f] [-n N] [service…]` | Logs, also kept in `.ncompose/logs/`. |
| `ncompose restart [service…]` | Restart services (all, if none named). |
| `ncompose config` | Validate the file and print the resolved plan, with warnings about binaries the shim cannot enter. |
| `ncompose net up\|down` | Add or remove this project's aliases by hand. |
| `ncompose system install\|uninstall\|status` | The boot-time alias pool. |

`NCOMPOSE_DEBUG=1` makes the shim log every rewrite to the service's stderr.

## Limits

ncompose works at the libc level, so:

- **SIP-protected binaries** (`/usr/bin/*`, `/System/*`) and **hardened-runtime** binaries ignore
  `DYLD_INSERT_LIBRARIES` and see the real network. `ncompose config` and `up` warn about them.
  Your own binaries and Homebrew's work. A hardened binary you control can be re-signed ad hoc:
  `codesign -f -s - <binary>`.
- **The filesystem is shared.** Unix sockets, lock files and data directories are not isolated.
  Give each service its own paths (for PostgreSQL, `unix_socket_directories`).
- **Wildcard listeners shadow service addresses.** Until a service has bound `ip:port`, any host
  process listening on `0.0.0.0:port` answers for it. Published ports and the proxy therefore
  default to `127.0.0.1`, and a healthcheck only passes once the service's own process group
  is listening.
- **Child processes** inherit the shim unless they clear their environment.
- No PID or mount namespaces, no resource limits, TCP-only publishing, IPv4 service addresses.

## Repository layout

| Path | |
|---|---|
| `crates/shim` | `libncompose_shim.dylib`, the interposition library |
| `crates/ncompose` | the CLI and supervisor |
| `crates/echo-server` | HTTP + WebSocket echo server, a native stand-in for `jmalloc/echo-server` |
| `crates/testsvc` | a tiny HTTP server for trying the networking out |
| `examples/demo` | three copies of `testsvc` on one port, with publishing and the proxy |
| `examples/dann` | a GraalVM native-image application (DANN) with a private PostgreSQL |
| `examples/dann-native-tests` | shared PostgreSQL for running DANN's native test suite on ncompose |

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
```

The shim's end-to-end tests (`crates/shim/tests/interposition.rs`) re-run the test binary as a
child with the freshly built dylib injected, and check the real calls: binds, name resolution,
DNS redirection, `localhost`. The tests that need loopback aliases skip themselves when the
machine has none.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any
additional terms or conditions.
