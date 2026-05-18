# Flaky Tests Under `--workspace` — 2026-05-18

All test files pass **reliably in isolation**. Flakes only appear under
`cargo test --workspace` where multiple test binaries spawn concurrent
daemons that race on port binding and tempdir allocation.

## Root Cause

`pick_free_port()` in `ryeosd/tests/common/mod.rs` binds `127.0.0.1:0`,
reads the assigned port, then drops the listener. Between the drop and
the child daemon binding the same port, another test can grab it
(TOCTOU race). Under `--workspace`, up to ~8 daemon-spawning test files
run in parallel, making this race likely.

## Affected Test Files (8 files, ~50 daemon-spawning tests)

| Test file | Tests | Typical failure mode |
|---|---|---|
| `scheduler_e2e.rs` | 19 tests (8 timed) | `daemon.json never appeared` (port race) or `at schedule timestamp is in the past` (slow spawn) |
| `service_data_e2e.rs` | 20 tests | `daemon.json never appeared` (port race) |
| `runtime_e2e.rs` | 9 tests | `daemon.json never appeared` (port race) |
| `dispatch_pin.rs` | 7 tests | `daemon.json never appeared` (port race) |
| `cleanup_e2e.rs` | ~13 tests | `daemon.json never appeared` (port race) |
| `build_bundle_smoke.rs` (ryeos-tools) | 2 tests | Same pattern |
| `service_data_standalone_e2e.rs` | 2 tests | Generally OK (subprocess, not daemon) |
| `service_dispatch_gates.rs` | 7 tests | **Not flaky** (no daemon spawn) |

## Typical Failure Messages

```
daemon.json never appeared at /tmp/.tmpXXXXXX/core/daemon.json — daemon stderr:
```
— Daemon child bound to a port that was grabbed by another test. Stderr
  is empty because the daemon exited immediately on bind failure.

```
scheduler.register at: expected 200, got 500 Internal Server Error;
body={"code":"internal","error":"internal: at schedule timestamp is in the past"}
```
— Under load, daemon startup takes >6s. By the time the test registers
  an `at` schedule 6s in the future, the timestamp has already passed.

```
expected at least 1 fire within 8s
```
— Scheduler timer loop didn't tick because the reload signal was lost
  (bounded channel full during concurrent startup) or the daemon itself
  failed to start (port race).

## Hardening Done (this session)

1. **Scheduler reload `try_send` failures now logged** — previously
   silently swallowed. Doesn't fix the race but makes it diagnosable.
2. **Raw `sleep(5s)` replaced with `observe_fire_count_stable`** —
   pause/deregister tests now poll `show_fires` at 3 points over 3s
   instead of sleeping 5s unconditionally.
3. **At-schedule lead time 3s → 6s** — reduces past-timestamp failures
   under load.

## Recommended Fixes (in priority order)

### P0: Fix `pick_free_port()` TOCTOU

Replace `bind(:0) → read port → drop → child binds same port` with
one of:

- **Option A**: Bind `:0`, keep the listener alive, pass the fd to the
  child via `SO_REUSE_PORT` or systemd socket activation protocol.
- **Option B**: Use a per-test-file port range allocated at file scope
  via `OnceLock<PortRange>` so files don't overlap.
- **Option C**: Use Unix domain sockets instead of TCP for test
  communication (no port allocation at all).

Option B is lowest effort. Option C is cleanest long-term.

### P1: Scheduler timed tests → separate binary

Split `scheduler_e2e.rs` into:
- `scheduler_crud_e2e.rs` — register/list/pause/resume/deregister/validation
  (no wall-clock dependence, ~11 tests)
- `scheduler_timed_e2e.rs` — at-schedule fires, interval fires,
  pause-prevents-fires, deregister-stops-fires, restart, fire-id
  determinism (~8 tests)

Run `scheduler_timed_e2e` with `--test-threads=1` or sequentially in CI.

### P2: In-process clock for scheduler integration tests

For the timed scheduler tests, the advanced path:
- Add injectable clock abstraction to `ryeos_scheduler::timer`
- Run `timer::run()` in-process with paused Tokio time
- Drive register/pause/resume/deregister deterministically
- Keep only 2-3 daemon E2E scheduler tests as smoke tests

This is L-effort but eliminates all timing flakiness.

### P3: Startup wait reliability

`start_fast()` waits for `daemon.json` to exist then polls `/health`.
Under heavy concurrency, the daemon may take longer to write
`daemon.json`. Consider:
- Increase `daemon.json` wait timeout (currently 15s — should be enough)
- Add a retry loop for the HTTP health check (already present, 5s)
- These are probably fine once the port race is fixed

## Reproduction

```bash
# Isolated — passes reliably
cargo test -p ryeosd --test scheduler_e2e

# Concurrent — flakes
cargo test --workspace 2>&1 | grep FAILED
```

Typical flake rate under `--workspace`: 3-8 tests per run, spread across
`scheduler_e2e`, `service_data_e2e`, `runtime_e2e`, `cleanup_e2e`.
