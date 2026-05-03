```yaml
id: cross-platform-support
title: "Cross-Platform Support"
description: Analysis of what it takes to run ryeosd and its runtimes on macOS, Windows, and via Docker — covering the Unix-specific code that must be abstracted, the effort involved, and a recommended path.
category: future
tags: [cross-platform, docker, windows, macos, portability, os-abstraction]
version: "0.1.0"
status: exploratory
```

# Cross-Platform Support

> **Status:** Exploratory — not on the roadmap, but the analysis is here for when it matters.

---

## Executive Summary

The Rust codebase is **overwhelmingly Unix-centric by design**. The daemon uses Unix domain sockets as its primary IPC mechanism, `libc` for process group management, and `flock` for advisory file locking. That said, most business logic (resolution, state, CAS, signing, graph walking) is pure Rust with no OS dependencies.

- **Docker for Linux**: 1 file, 0 code changes, ship today.
- **macOS support**: ~100–150 lines of `cfg` guards. macOS has UDS + `flock` + signals, so most things just work.
- **Windows support**: ~300–500 lines. The UDS→transport abstraction is the main work, plus process group management.

The `lillux` crate already has full Windows parity and serves as the template for how to structure dual-platform code.

---

## Docker (Linux Container)

Trivially achievable today. A `.dockerignore` already exists.

```dockerfile
FROM rust:1-slim-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p ryeosd -p ryeos-cli -p ryeos-directive-runtime -p ryeos-graph-runtime

FROM debian:bookworm-slim
COPY --from=build /src/target/release/ryeosd /usr/local/bin/
COPY --from=build /src/target/release/rye /usr/local/bin/
COPY --from=build /src/target/release/ryeos-directive-runtime /usr/local/bin/
COPY --from=build /src/target/release/ryeos-graph-runtime /usr/local/bin/
EXPOSE 9090
ENTRYPOINT ["ryeosd"]
```

No code changes required. The entire codebase already compiles and runs on Linux.

For multi-arch (ARM64 for Apple Silicon / AWS Graviton), add `--platform=linux/arm64` or use `docker buildx`.

---

## OS-Specific Code Inventory

### 1. Unix Domain Sockets (the big one)

The primary IPC transport between `ryeosd` and the runtimes. **No TCP or named-pipe fallback exists.**

| File                                 | Usage                                                         |
| ------------------------------------ | ------------------------------------------------------------- |
| `ryeosd/src/main.rs:271`             | `UnixListener::bind(&config.uds_path)` — daemon listens       |
| `ryeosd/src/uds/server.rs:20`        | `serve(listener: UnixListener, ...)` — accept loop            |
| `ryeos-runtime/src/daemon_rpc.rs:80` | `UnixStream::connect(&self.socket_path)` — runtime RPC client |
| `ryeos-runtime/src/callback_uds.rs`  | Entire `UdsRuntimeClient` module — callback path              |

**What to do:** Introduce a transport abstraction trait:

```rust
trait Transport {
    async fn connect(path: &str) -> Result<Self>;
    async fn read_frame(&mut self) -> Result<Vec<u8>>;
    async fn write_frame(&mut self, data: &[u8]) -> Result<()>;
}
```

- Unix: UDS implementation (current code, unchanged)
- Windows: named pipes (`\\.\pipe\ryeosd`) via `tokio::net::windows::named_pipe`
- Could also support TCP loopback as a universal fallback

**Effort:** Medium. The framing layer (`read_frame`/`write_frame`) is already protocol-agnostic — it just needs to be lifted off `UnixStream` onto the trait.

---

### 2. Unguarded Unix APIs (won't compile on Windows)

These will fail to compile on Windows because they use Unix-only APIs without `#[cfg(unix)]` guards:

| File                                  | Line(s) | Problem                                                                       |
| ------------------------------------- | ------- | ----------------------------------------------------------------------------- |
| `ryeos-directive-runtime/src/main.rs` | 122–124 | `tokio::signal::unix::signal(SignalKind::terminate())` — no cfg guard         |
| `ryeos-state/src/gc/lock.rs`          | 50, 106 | `libc::flock()` + `AsRawFd` — unconditional, no cfg gate on `libc` dep either |
| `ryeos-runtime/src/daemon_rpc.rs`     | 80      | `tokio::net::UnixStream::connect()` — unconditional                           |

**What to do:** Add `#[cfg(unix)]` guards and provide `#[cfg(not(unix))]` stubs that either no-op (signals, locking) or delegate to the transport abstraction (UDS connect). The `ryeos-state/Cargo.toml` also needs `libc` moved to a `[target.'cfg(unix)'.dependencies]` section.

**Effort:** Low — ~30 lines total.

---

### 3. Process Groups / Signals

The daemon manages child processes using Unix process groups via `libc`:

| File                    | Usage                                                                        |
| ----------------------- | ---------------------------------------------------------------------------- |
| `ryeosd/src/process.rs` | `libc::kill(-(pgid), SIGTERM/SIGKILL)`, `libc::getpgid()`, `libc::setpgid()` |
| `lillux/src/exec.rs`    | `libc::setsid()` via `pre_exec`, `libc::kill()` for timeouts                 |

`lillux` **already has Windows implementations** for `spawn_detached`, `kill_process`, and `is_alive` using `windows-sys` (`OpenProcess`, `TerminateProcess`, `CREATE_NEW_PROCESS_GROUP`). This is the template to follow.

**What to do:** Apply the same `lillux` pattern to `ryeosd/src/process.rs` — dual `#[cfg(unix)]` / `#[cfg(windows)]` blocks.

**Effort:** Low-medium — `lillux` proves the approach works.

---

### 4. File Permissions

| File                                                 | Usage                                                                 |
| ---------------------------------------------------- | --------------------------------------------------------------------- |
| `ryeosd/src/config.rs:224`                           | Sets directory mode `0o700`                                           |
| `ryeosd/src/main.rs:297`                             | Sets socket mode `0o600`                                              |
| `ryeosd/src/execution/ingest.rs:30`                  | Reads exec bit via `permissions().mode()`                             |
| `ryeosd/src/services/handlers/bundle_install.rs:348` | `std::os::unix::fs::symlink()` — already has `#[cfg(not(unix))]` bail |
| `lillux/src/cas.rs:44`                               | `materialize_executable()` — already guarded                          |
| `lillux/src/identity/keypair.rs:131`                 | `set_perms()` — already guarded                                       |

**What to do:** Most of these already have cfg guards. The remaining ones (`config.rs`, `ingest.rs`) just need the same treatment. Windows doesn't need POSIX permission modes — the key requirement (restricting access to the owning user) is handled by NTFS ACLs if needed, or can be skipped for dev scenarios.

**Effort:** Low.

---

### 5. UID Queries

| File                                  | Usage                                                                      |
| ------------------------------------- | -------------------------------------------------------------------------- |
| `ryeosd/src/config.rs:11`             | `libc::geteuid()` — already has `#[cfg(not(unix))]` fallback returning `0` |
| `ryeosd/src/standalone_audit.rs:90`   | Same pattern — already guarded                                             |
| `ryeos-runtime/src/daemon_rpc.rs:333` | `libc::getuid()` for socket path — already has non-Unix fallback           |

**Effort:** Already handled. No work needed.

---

### 6. Path Conventions

The codebase uses the `directories` and `dirs` crates, which already resolve to the correct platform-specific paths (XDG on Linux, `Library/Application Support` on macOS, `AppData` on Windows). However, there are hardcoded Unix fallback paths:

| Hardcoded path                  | File(s)                                                      | Purpose                  |
| ------------------------------- | ------------------------------------------------------------ | ------------------------ |
| `/tmp/ryeosd-{uid}/ryeosd.sock` | `daemon_rpc.rs:336`                                          | Socket fallback          |
| `/tmp/ryeosd.sock`              | `daemon_rpc.rs:340`                                          | Non-Unix socket fallback |
| `/var/lib/ryeosd`               | `standalone_audit.rs`, `state_lock.rs`, `launch_metadata.rs` | Default state dir        |
| `/tmp/missing-home`             | `bootstrap.rs:50`                                            | Fallback home            |
| `~/.ai/config/keys/signing/`    | `keypair.rs`, `bootstrap.rs`                                 | Key path                 |

**What to do:** Replace hardcoded paths with `directories` crate lookups. On Windows, use named pipe paths instead of socket file paths.

**Effort:** Low — the `directories` crate already does the heavy lifting.

---

### 7. File Locking (`flock`)

| File                            | Usage                                         | Guarded?                 |
| ------------------------------- | --------------------------------------------- | ------------------------ |
| `ryeosd/src/state_lock.rs:78`   | `libc::flock(fd, LOCK_EX \| LOCK_NB)`         | Yes — has no-op fallback |
| `ryeos-state/src/gc/lock.rs:50` | `libc::flock(lock_file.as_raw_fd(), ...)`     | **No** — unconditional   |
| `ryeos-state/src/chain.rs:91`   | `libc::flock(lock_file.as_raw_fd(), LOCK_EX)` | Yes — has `#[cfg(unix)]` |

**What to do:** Add cfg guards to `ryeos-state/src/gc/lock.rs`. On Windows, use `LockFileEx` / `UnlockFileEx` via `windows-sys`, or use a cross-platform crate like `fs4` or `file-lock`.

**Effort:** Low.

---

## Dependency Summary

| Crate                 | OS-specific dep                        | Gated?                  |
| --------------------- | -------------------------------------- | ----------------------- |
| `ryeosd`              | `libc`                                 | Yes (`cfg(unix)`)       |
| `ryeos-runtime`       | `libc`                                 | Yes (`cfg(unix)`)       |
| `ryeos-state`         | `libc`                                 | **No** — unconditional  |
| `ryeos-graph-runtime` | `libc`                                 | Yes (`cfg(unix)`)       |
| `lillux`              | `libc` (Unix), `windows-sys` (Windows) | Yes — full dual support |
| `ryeos-cli`           | `dirs`                                 | Platform-aware crate    |
| `ryeosd`              | `directories`                          | Platform-aware crate    |
| `ryeos-engine`        | none                                   | Pure Rust               |
| `ryeos-tracing`       | none                                   | Pure Rust               |

---

## Recommended Approach

### Phase 1: Docker (zero risk)

Ship a Dockerfile. No code changes. Users on any OS can run `docker run ryeosd` immediately. This covers the "works on my machine" problem and gives Windows/macOS users a path today.

### Phase 2: macOS (low effort)

macOS has UDS, `flock`, POSIX signals, and `libc`. The unguarded `#[cfg(unix)]` blocks all work on macOS. The main work is:

1. Fix the 3 unguarded compilation errors (~30 lines)
2. Gate `libc` in `ryeos-state/Cargo.toml` (1 line)
3. Test

Estimated: ~100–150 lines changed.

### Phase 3: Windows (medium effort)

1. Introduce the transport abstraction trait for UDS → named pipes
2. Apply `lillux`-style dual cfg blocks to `ryeosd/src/process.rs`
3. Add cfg guards to remaining unguarded files
4. Replace hardcoded Unix paths with `directories` lookups
5. Handle file locking (use `fs4` crate or inline Windows API calls)

Estimated: ~300–500 lines changed, concentrated in `ryeosd` and `ryeos-runtime`.

### Phase 4: CI matrix

Add Windows and macOS targets to `.github/workflows/publish.yml`:

```yaml
strategy:
  matrix:
    include:
      - os: ubuntu-latest
        target: x86_64-unknown-linux-gnu
      - os: macos-latest
        target: x86_64-apple-darwin
      - os: macos-latest
        target: aarch64-apple-darwin
      - os: windows-latest
        target: x86_64-pc-windows-msvc
```

---

## What Does NOT Need to Change

- `ryeos-engine` — pure Rust, no OS dependencies
- `ryeos-tracing` — pure Rust
- `ryeos-state` — mostly pure Rust (just the `gc/lock.rs` fix)
- `ryeos-graph-runtime` — business logic is platform-independent
- `ryeos-directive-runtime` — business logic is platform-independent
- `lillux` — already has Windows support
- All CAS, signing, verification, chain, and projection logic
