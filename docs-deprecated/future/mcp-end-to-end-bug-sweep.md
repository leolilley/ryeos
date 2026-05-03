# MCP-driven end-to-end bug sweep — open issues

Status: in-progress (handoff doc)
Branch for fixes: `feat/route-webhooks-and-gateway` (worktree
`/home/leo/projects/ryeos-cas-as-truth-routes`).
Driver: `mcp__rye__cli` (the `ryeosd-mcp` thin wrapper subprocessing to
the data-driven `rye` Rust CLI). Tests for the wrapper itself are green
(13/13). The wrapper is correct — bugs surface in the CLI YAMLs and the
daemon.

This document is the running list of issues discovered while driving the
whole Rye system through that single MCP tool. It is intended to be
read by another agent and executed verb-by-verb. Hard project rules
apply throughout: NO backwards compatibility, NO legacy refs, NO
silent fallbacks, NO migration shims, NO `#[ignore]`, NO FIXMEs, NO
`#[allow(dead_code)]` in route-system code, NO vendor names outside
test fixtures, NO hardcoded kind names in the daemon, typed body
deserialization (`#[serde(deny_unknown_fields)]`), typed-fail-loud
everywhere.

Each issue lists: status, location, observed behaviour, root cause when
known, and the policy-clean fix (no compatibility shims).

---

## Closed in this session

### B1. `bundle-remove.yaml` carried false description text — FIXED
- File: `ryeos-bundles/standard/.ai/config/cli/bundle-remove.yaml`.
- Description claimed "(offline-only; spawns `ryeosd run-service`)".
  Untrue: the verb routes to a daemon handler, never spawns a
  standalone runner.
- Fix: rewrote the description, re-signed via
  `cargo run --example resign_yaml -p ryeos-engine -- <abs-path>`
  using the user signing key (fingerprint `8f4c0023…`).

### B2. `thread-tail.yaml` referenced a non-existent service — FIXED
- File: `ryeos-bundles/standard/.ai/config/cli/thread-tail.yaml`.
- Verb declared `service:threads/tail` but no daemon handler with that
  `service_ref` exists in `ryeosd/src/services/handlers/`.
- Fix: deleted the YAML. SSE thread-tail can be reintroduced when the
  handler lands. ryeosd test count remains 438/0/0 unchanged. The
  17-verb operational set after deletion is below.

### B3. `RuntimeDb::open()` corrupted on stale schema — FIXED
- File: `ryeosd/src/runtime_db.rs`.
- `RuntimeDb::open` blindly ran `execute_batch(SCHEMA_SQL)` (a batch
  containing `CREATE TABLE IF NOT EXISTS thread_runtime …; CREATE INDEX
  … ON thread_runtime(chain_root_id);`). If a stale db file at
  `~/.local/state/ryeosd/db/ryeosd.sqlite3` was present with a
  pre-`chain_root_id` `thread_runtime` table, the `CREATE TABLE`
  no-op'd, then the `CREATE INDEX` failed mid-batch with `no such
  column: chain_root_id`, leaving the file half-initialised.
- Fix (final, no version-stamp / no legacy detection): before applying
  DDL, count rows in `sqlite_master` of type `'table'`. Zero → fresh
  init, run DDL. Non-zero → `verify_owned_schema` walks a fixed list
  of `(table, column)` pairs that THIS daemon's schema requires
  (including `thread_runtime.chain_root_id`,
  `thread_runtime.launch_metadata`, `thread_runtime.resume_attempts`,
  `thread_commands.command_id` …). Any missing column → bail loud:
  "runtime db at <path> was not created by this daemon; refusing to
  operate on foreign schema. Archive or delete the state directory
  and re-init."
- Tests added: `refuses_db_with_foreign_tables`,
  `refuses_db_missing_expected_column`. ryeosd test count 438 → 440,
  all green.
- Operator action when this fires: archive or delete the state dir;
  the daemon owns its dir.

### B4. MCP tool name `mcp__rye__rye` was awkward — FIXED
- File: `ryeosd-mcp/ryeosd_mcp/server.py`.
- Renamed `TOOL_NAME = "rye"` → `TOOL_NAME = "cli"`. New tool ID is
  `mcp__rye__cli`. Server name stays `"rye"` so all 13 wrapper tests
  continue to pass.

---

## Recommended execution order

Work in this order — dependency-graphed, not alphabetic:

1. **O3.E** — runtime-side resume refusal (temporary safety stop;
   isolates the unsafe path while everything else lands).
2. **O1** — config + bind + `deny_unknown_fields` (subsumes O8).
3. **O2** — daemon-wide SQLite schema-ownership invariant.
4. **O5 + O6** — runtime_db corruption fail-loud + reconcile
   resume_attempts.
5. **O3.B** — typed event wire-shape (must precede O3.A so the
   typed `thread_usage` event is reachable from both crates).
6. **O3.A + O3.D** — typed `ThreadUsage`, settlement contract
   (subsumes O9).
7. **O3.C** — callback-method classification + fail-loud path.
8. **O7** — RuntimeCost round-trip in BOTH caller envelope AND
   durable finalisation.
9. **O11** — confirm + test no-fallback UDS callback failure path.
10. **O12 + O13** — strict serde on RPC params + remove silent
    `.ok()` decode losses.
11. **O10** — envelope-shape contract audit.
12. **O4a** — boot smoke pass after O1/O2 ship.
13. **O4b** — full 17-verb walk after everything else.

The primary correctness work is O3.A–E; the rest are fail-loud
hardening that should become trivial once the contract types
exist.

## Open issues (priority below; execute in the order above)

### O1. `--bind` CLI flag conflicts with existing config silently — HIGH

Oracle review corrected the root-cause analysis here.

- Likely real cause:
  - `Config::load` seeds defaults from `cli.bind`
    (`ryeosd/src/config.rs:104`, `199-215`), so a fresh
    `--init-only --bind X` with no pre-existing `config.yaml` does
    write `X`.
  - With an existing `config.yaml`, file-side `bind` wins over CLI
    (`config.rs:128-131`) AND `bootstrap::init` only rewrites the
    file when missing or `--force` (`bootstrap.rs:29-33`). So the
    operator's `--bind 127.0.0.1:7401` is silently overridden by the
    stored `127.0.0.1:7400`.
  - Adjacent fail-loud violation: `Self::load_file(&default_config).ok()`
    swallows YAML parse errors (`config.rs:109-110`).
  - Adjacent shape violation: `PartialConfig` lacks
    `#[serde(deny_unknown_fields)]` (`config.rs:90-100`), so unknown
    keys silently parse.
- Required action:
  1. If `config.yaml` already exists AND `--bind` was supplied AND
     it disagrees with the file value, **error loudly** ("conflict
     between CLI --bind and stored config.yaml; pass --force to
     overwrite"). No silent precedence either direction.
  2. Replace `.ok()` on the default-config load with a typed bail.
  3. Add `#[serde(deny_unknown_fields)]` to `PartialConfig` and
     every nested config struct.
  4. Tests: fresh-init with `--bind` writes the CLI value;
     existing-config + conflicting `--bind` errors; existing-config
     + matching `--bind` is accepted; malformed `config.yaml`
     refuses to start.
- Acceptance: `mcp__rye__cli` driving init with `--bind` either
  writes the value (fresh) or errors loudly (conflict). No silent
  override, ever.

### O2. Daemon-wide SQLite schema-ownership invariant — HIGH

Oracle review noted the column-list approach in B3 is too weak as a
template for a daemon-wide invariant: it only proves "these columns
exist somewhere", not that the schema is exactly ours. The
projection.db fix needs to be the moment we tighten that.

- Affected files (current):
  - `ryeos-state/src/projection.rs:150-156` — `Connection::open` →
    unconditional `execute_batch(SCHEMA_SQL)`. Same partial-DDL hazard
    that produced B3.
  - `ryeosd/src/runtime_db.rs:88-115` — fixed in B3 but with a
    column-presence check that lets foreign tables, wrong column
    types, missing indexes, and unexpected columns slip through.
- Required action — make schema ownership a daemon-wide invariant:
  1. New shared module `ryeos-state::sqlite_schema` with API:
     ```rust
     pub struct SchemaSpec {
         pub application_id: i32,         // PRAGMA application_id stamp
         pub tables: &'static [TableSpec],
         pub indexes: &'static [IndexSpec],
     }
     pub fn assert_owned(conn: &Connection, spec: &SchemaSpec) -> Result<()>;
     pub fn init_owned(conn: &Connection, spec: &SchemaSpec, ddl: &str) -> Result<()>;
     ```
     `TableSpec` carries ordered column names + declared types + PK
     + NOT NULL flags. `IndexSpec` carries name + table + columns.
  2. `assert_owned` checks (in this order, fail-loud at each step):
     - `PRAGMA application_id` matches `spec.application_id`. Fast
       reject for non-ours files.
     - For each `TableSpec`: `PRAGMA table_info(table)` returns the
       exact ordered column set with matching types/pk/notnull.
     - For each `IndexSpec`: `PRAGMA index_info` matches.
     - No unexpected **user** tables/indexes. CRITICAL: ignore
       SQLite internal objects — `sqlite_master.name LIKE 'sqlite_%'`
       (covers `sqlite_sequence` from `runtime_db.rs:57-58`'s
       `AUTOINCREMENT`) and any `sqlite_autoindex_*` rows generated
       by PK/UNIQUE constraints (e.g. projection's
       `UNIQUE(thread_id, key)` at `projection.rs:123-130`). Reject
       extras only among the user-owned set.
  3. `init_owned` runs the DDL on a verified-empty file and stamps
     `PRAGMA application_id`.
  4. Replace `RuntimeDb::open` body's column-list check (and B3's
     `REQUIRED_COLUMNS` constant) with a `SchemaSpec` and a single
     `assert_owned` call. Apply the same shape to
     `ProjectionDb::open`. Each crate owns its own `SchemaSpec`.
  5. Choose stable `application_id` values per DB (e.g.
     `0x52594541` "RYEA" for runtime, `0x5259504a` "RYPJ" for
     projection — these are local cache files, NOT CAS objects, so
     a numeric tag is fine).
- Out of scope: a CAS-backed schema manifest. These are local
  caches, not authoritative state; over-engineering that has no
  win.
- Tests: mirror B3's two refusal tests for projection; add a test
  for "extra unexpected table" rejection on both runtimes.
- Acceptance: any sqlite file in `state_dir` whose
  `application_id`, table set, column set, or index set deviates
  from the declared spec causes daemon startup to fail loud with a
  structured error pointing at the offending diff.

### O3. Directive-runtime resume is structurally unsafe — HIGH

This is the deeper miss. The "budget loss on resume" headline was
just the most visible symptom; the resume path has multiple
independent silent-fallbacks and is not safely recoverable today.
Two orthogonal fixes are required, then a refuse-by-default policy
until both land.

NOTE: `docs/future/native-runtimes.md` and
`docs/future/RESUME-ADVANCED-PATH.md` exist but are stale. Do NOT
anchor implementation on them. The CAS objects, callback shape, and
DDL referenced below are what's actually in the workspace today.

#### O3.A — All per-thread accumulators reset on resume

Today every per-thread limit lives in two in-process structs that
the resume branch reconstructs from scratch:

- `BudgetTracker` (`ryeos-directive-runtime/src/budget.rs:11-18`):
  `total_input`, `total_output`, `total_usd`, `max_usd`. Reset by
  `BudgetTracker::new` in
  `ryeos-directive-runtime/src/main.rs:130`. The fresh tracker is
  threaded straight into `Runner::from_resume`
  (`main.rs:134-151`). On `runner.rs:251` it accumulates per-turn
  cost, on `runner.rs:231` it gates `is_exhausted`.
- `Harness` (`ryeos-directive-runtime/src/harness.rs:13-18`):
  `turns_used`, `tokens_used`, `spend_used`, `spawns_used`, `start`.
  All reset in `Harness::new` (`harness.rs:23-35`), enforced in
  `check_limits` (`harness.rs:42-82`). Resume only restores
  `runner.initial_turn` (`runner.rs:154-168`); nothing reseeds
  Harness.

Effect: a directive that crashes at $0.99 of a $1.00 cap, or at
turn 49 of a 50-turn cap, or at the duration limit, restarts at
zero on every resume. Continuation gating in `runner.rs:380-387`
also reads `self.budget.cost()`, so token-driven "should I keep
going" answers change after resume too.

The `ThreadSnapshot` CAS object already has the slots we need:

- `ryeos-state/src/objects/thread_snapshot.rs:146` —
  `pub budget: Option<serde_json::Value>`
- `ryeos-state/src/objects/thread_snapshot.rs:149` — typed
  `facets: BTreeMap<String,String>` (the doc-comment example shows
  `cost.spend`, `cost.tokens`).

Today every production write of `ThreadSnapshot` sets
`budget: None` and an empty `facets` map
(`ryeosd/src/state_store.rs:204-206`, also lines 404/495/597).
This is the silent-fallback to repair.

Canonical naming (used uniformly below and in acceptance):
the typed value is `ThreadUsage`; the slot on the snapshot stays
named `budget` (existing field in `ThreadSnapshot` line 146); the
event vocabulary entry is `thread_usage`; the CLI surface block
is `usage`. One concept, three names per layer, fixed throughout.

Required action:

1. Define a typed `ThreadUsage { completed_turns: u32, input_tokens:
   u64, output_tokens: u64, spend_usd: f64, spawns_used: u32,
   started_at: String, settled_at: String, last_settled_turn_seq:
   u64, elapsed_ms: u64 }` in `ryeos-state::objects` and replace
   `ThreadSnapshot.budget`'s `Option<serde_json::Value>` with
   `Option<ThreadUsage>`. `#[serde(deny_unknown_fields)]`. No
   external readers depend on the loose JSON shape today.
2. Add a `thread_usage` variant to the runtime ↔ daemon event
   vocabulary at `ryeos-runtime/src/events.rs` (or wherever the
   typed event-type enum lives — fresh LLM: search before adding).
   `#[serde(deny_unknown_fields)]`.
3. The directive runtime callback (`runner.rs:254-263`) must emit
   `thread_usage` at every settled turn boundary, carrying the
   cumulative `ThreadUsage`. Use the SAME callback-RPC method that
   appends events; do not invent a new transport.
4. Daemon-side handler (`ryeosd/src/uds/server.rs:174-185`,
   `ryeosd/src/state_store.rs::finalize_thread` / equivalent
   snapshot-updating code around lines 486-519) atomically
   appends the event AND updates `ThreadSnapshot.budget` in a
   single durable settlement before ACKing the callback (see
   O3.D for the strict ordering contract). The handler is keyed
   on event type, NOT on `kind_name` — daemon stays
   kind-agnostic; any runtime that emits a `thread_usage` event
   gets its snapshot updated identically.
5. `Runner::from_resume`
   (`ryeos-directive-runtime/src/runner.rs:154-168`) accepts an
   `Option<ThreadUsage>` and reseeds both `BudgetTracker` and
   `Harness` from it. Extend `Harness::new`'s signature to take
   `prior_usage: Option<&ThreadUsage>` (do NOT introduce a parallel
   `Harness::resume` constructor — there's no v2 vs v1 here, just
   a richer init).
6. The resumed runner re-evaluates `is_exhausted` and Harness
   `check_limits` on construction — a thread that was over the
   cap at crash must NOT get a free turn.
7. Read path: extend `ThreadDetail` (or whatever
   `ryeosd/src/services/thread_lifecycle.rs` returns from
   `runtime.get_thread`) with a typed `usage: Option<ThreadUsage>`
   field so `mcp__rye__cli args=["thread","get","T-…"]` surfaces
   it. Today `ThreadDetail` does not expose budget/usage at all.
8. Projection write path: `ryeos-state/src/sync.rs` stops
   hard-coding `"budget": null` in production envelope-build
   (the three references at lines 405 / 561 / 637 are currently
   test fixtures, but the matching production callsites must
   surface the typed value too).

#### O3.B — The resume parser reads legacy event shape

`ryeos-directive-runtime/src/resume.rs:19-23` and `40-94` expect
events with `events[*].type` and `events[*].data`. The daemon's
replay service returns `PersistedEventRecord` with `event_type` and
`payload` (`ryeosd/src/services/event_store.rs:52-55`); the
runtime-side transcript reconstruction in `ryeos-runtime/src/transcript.rs:151-154`
also expects `event_type`/`payload`.

The current resume parser silently drops every replayed event
because the field names don't match, then defaults missing
`events` to `[]` (`resume.rs:19-23`) and ignores unknown event
types (`resume.rs:94`). The unit tests in `resume.rs:191-284`
construct fake events with `"type"`/`"data"`, so they pass while
the production resume path is broken.

Required action:

1. The current `PersistedEventRecord` lives in
   `ryeosd/src/services/event_store.rs:14-49` — the runtime crate
   cannot depend on `ryeosd`. Resolve by EITHER:
   a. Promote the wire type into `ryeos-runtime` (preferred —
      it's already shared callback-contract territory), OR
   b. Define a mirror `ReplayedEventRecord` in `ryeos-runtime`
      with the same fields and `#[serde(deny_unknown_fields)]`,
      and add a single typed conversion at the daemon's
      replay-emit boundary. The daemon's `PersistedEventRecord`
      and the runtime's mirror MUST be cross-tested with a
      shared serde fixture so they cannot drift.
   Either way, no inline JSON-shaped reader on the runtime side.
2. Use that typed shape in `resume.rs` instead of the hand-rolled
   field walks at `resume.rs:40-94`.
3. Remove the `events.unwrap_or(vec![])`-style fallback at
   `resume.rs:19-23`. Missing `events` → bail.
4. Remove the silent skip on unknown event types at
   `resume.rs:94`. Unknown variant → bail.
5. Rewrite `resume.rs:191-284` tests to use the typed shape. Add
   a fixture loaded from a `serde_json::json!` produced by the
   daemon side (e.g. via a `cfg(test)`-only re-export) so the
   two crates' shapes are kept in lockstep at test time.

#### O3.C — Callback methods silently no-op when disconnected

The directive runtime treats persistence failures as warnings:

- `record_callback_warning` (`runner.rs:81-92`) logs and continues.
- The same path is used for `emit_turn_complete` (`runner.rs:254-263`),
  `stream_opened`, `tool_dispatch`, `tool_result`.
- `CallbackClient::replay_events_for` returns `Value::Null` when
  disconnected (`ryeos-runtime/src/callback_client.rs:220-225`).
- `append_event`, `mark_running`, `finalize_thread` no-op when
  disconnected (`callback_client.rs:122-131`, `163-190`, `228-233`).

If durable state is the source of truth for resume, no
resume-critical event can be allowed to vanish silently. Required
action:

1. Categorise every method on `CallbackClient`
   (`ryeos-runtime/src/callback_client.rs`) as either
   **resume-critical** (must hard-fail on disconnect) or
   **advisory** (warn-and-continue is acceptable). Walk the full
   public method set; do not partially classify.
2. Resume-critical (must hard-fail): `thread_usage`,
   `mark_running`, `finalize_thread`, `replay_events_for`, and
   `append_event` for transcript-bearing event types
   (`cognition_in`, `cognition_out`, `tool_dispatch`,
   `tool_result`).
3. Advisory (warn-and-continue OK): purely cosmetic / progress
   events with no resume implications, e.g. `stream_opened`,
   `emit_turn_start` without usage payload. Document the
   classification of each method inline above its definition.
4. Resume-critical methods drop the warn path; failures propagate
   as typed errors. The runner exits the turn loop and returns
   `RuntimeResult { success: false, status: "errored", result:
   Some("callback persistence failed: <typed reason>"), … }`. Note
   `RuntimeResult` has no `cancelled` field
   (`ryeos-runtime/src/envelope.rs:120-145`); use `status =
   "errored"` with a typed result message. The daemon's
   reconciler treats it like any other failure finalisation.

#### O3.D — Crash window between in-memory accumulation and durable settlement

`runner.rs:247-252` mutates `harness` and `budget` BEFORE calling
`emit_turn_complete` at `runner.rs:254-263`. A SIGKILL/segv/OOM
between those two points executes the provider call, charges the
usage in-memory, never persists, and the resumed runner sees the
prior turn as "didn't happen" — even though the provider did spend.

Required action: invert the order. Build the cumulative
`ThreadUsage` value first, emit the durable `thread_usage`
callback, and ONLY update the in-process `Harness` /
`BudgetTracker` after a successful ACK. The daemon's settlement
contract is strict:

- Daemon receives the `thread_usage` callback.
- Daemon BOTH appends the typed event to the durable event log
  AND updates `ThreadSnapshot.budget` in CAS — as ONE
  write-barrier-protected unit (use the existing
  `ryeosd::write_barrier::WriteBarrier`). Both must commit
  together or neither commits.
- ONLY after both are durably committed does the daemon ACK
  the callback.
- ONLY after that ACK is the live event broadcast emitted
  (persistence-first invariant).
- If the durable write fails, the runner exits with the typed
  error from O3.C without charging local accounting; the next
  resume rebuilds accounting from CAS truth.

Note: today the daemon's normal append/publish path
(`ryeosd/src/uds/server.rs:174-185`) already persists before
publishing — but it does NOT currently couple the event append
with a snapshot update. That coupling is the new piece O3.D
delivers.

#### O3.E — Until O3.A–D land, the runtime itself refuses to resume

The daemon must STAY kind-agnostic — gating auto-resume by
matching `kind_name == "directive"` in `reconcile.rs` would
violate the "NO hardcoded kind names in daemon" rule. Push the
refusal down into the runtime that actually understands its own
state.

Required action:

1. In `ryeos-directive-runtime/src/main.rs::run_with_envelope`,
   when `envelope.request.previous_thread_id.is_some()`, the
   runtime checks for a typed `ThreadUsage` (per O3.A) AND
   typed-event-shape (per O3.B). If either prerequisite is
   unmet, the runtime returns
   `Ok(RuntimeResult { success: false, status: "errored",
   result: Some("resume prerequisites unmet: <typed reason>"),
   … })`. The runtime exits with code 0 — current
   `main.rs:23-31` exits 0 on `Ok(_)` and 1 on `Err(_)`. A failed
   `RuntimeResult` is a normal runtime outcome, not a runner
   panic, so it must take the `Ok` path so the daemon receives
   the JSON envelope on stdout.
2. The daemon's reconciler treats that the same as any other
   `success: false` runtime outcome — finalize-as-failed via the
   normal path. No special-case branch in `reconcile.rs`. No
   kind name in the daemon.
3. When O3.A–D ship, the runtime-side check stops firing because
   prerequisites are met; no daemon change required.

This keeps the temporary safety gate co-located with the broken
code that needs the gate, and removes itself when the underlying
fixes land — no leftover kind-aware match arm in `reconcile.rs`
to remember to delete.

Acceptance for the whole of O3:

- `mcp__rye__cli args=["thread","get","T-…"]` returns a typed
  `usage` block on multi-turn directives with cumulative cost +
  turn + duration counters.
- Crash-and-resume: a directive killed at any provider-call
  boundary resumes with prior `ThreadUsage` reseeded into both
  `BudgetTracker` and `Harness`. Over-budget threads at crash
  time refuse to start.
- `ryeos-directive-runtime/src/resume.rs` deserialises real
  `PersistedEventRecord` events; legacy `type`/`data` test
  fixtures deleted.
- No callback method on the resume-critical list silently
  no-ops; disconnected callback returns typed error.
- `cargo test -p ryeosd -p ryeos-state -p ryeos-directive-runtime
  -p ryeos-runtime --no-fail-fast` all green; the 13
  callback-client tests are EXPECTED to fail / be rewritten in
  O3.C — they currently encode the no-op-on-disconnect
  behaviour.

### O5. `launch_metadata` corruption silently degrades to `None` — MEDIUM

- File: `ryeosd/src/runtime_db.rs::get_runtime_info` lines 174-205.
- Schema-version mismatch and JSON decode failures both `warn!` and
  return `None`. The comment honestly states the consequence:
  "resume eligibility and cancellation routing disabled for this
  thread until the row is rewritten." That's a silent-fallback the
  operator only sees in logs.
- Required action:
  1. `bail!` on either branch. The caller treats a missing
     thread row as a normal not-found case; force the same loud
     surface for corruption.
  2. Replace the existing `garbage_launch_metadata_decodes_to_none_without_panic`
     (`runtime_db.rs:425-441`) and
     `schema_version_mismatch_yields_none_with_warn`
     (`runtime_db.rs:479-496`) tests with versions that assert
     a typed error is returned.
- Acceptance: corrupt or schema-mismatched launch_metadata row
  causes `get_runtime_info` to error; the only `None` path is
  the legitimate "no row" case.

### O6. Missing `resume_attempts` silently becomes zero — MEDIUM

- Files:
  - `ryeosd/src/runtime_db.rs::get_resume_attempts` lines 215-226
    (`unwrap_or(0).max(0) as u32`)
  - `ryeosd/src/reconcile.rs:189-199` (warn-and-treat-as-zero on
    read failure during resume-budget evaluation)
- A thread row that exists with no `resume_attempts` value is
  read as 0. With B3's strict schema check that case is
  impossible at the column level, but the `unwrap_or(0)` is
  still a silent fallback for any future row-level corruption.
  Reconcile then compounds it by treating an I/O error on the
  read path as "use 0" rather than "this thread can't be
  resumed safely".
- Required action:
  1. `get_resume_attempts` distinguishes "row missing"
     (legitimate fresh thread → 0) from "row present but
     `resume_attempts` NULL or unreadable" (corruption → bail).
  2. `reconcile.rs:189-199` propagates the bail rather than
     warning; the affected thread is finalize-as-failed,
     not silently retried with a fabricated counter.
- Tests: `get_resume_attempts` on a missing row returns 0;
  on a row with NULL `resume_attempts` errors; reconcile
  finalises-as-failed when the read errors.

### O7. RuntimeCost is dropped at TWO layers, not one — MEDIUM

Oracle review pointed out my original framing was incomplete.

- Layer 1 — caller-facing JSON: `ryeosd/src/execution/launch.rs:577-585`
  composes the launch envelope with
  `success/status/result/outputs/warnings` and omits
  `runtime_result.cost`. The MCP/CLI caller never sees cost.
- Layer 2 — durable finalisation:
  `ryeosd/src/services/thread_lifecycle.rs:371-380` and
  `ryeosd/src/state_store.rs:486-519` (terminal snapshot write)
  hard-code `budget: None` even when the runtime returned a
  real `RuntimeCost`. The CAS truth is silently empty.
- Required action — patch BOTH layers in one change:
  1. Layer 1: extend the caller-facing launch envelope with
     a typed `cost: Option<ThreadUsage>` (the same type
     defined in O3.A — single canonical shape).
  2. Layer 2: at every terminal-snapshot write, populate
     `ThreadSnapshot.budget` from the same `RuntimeCost`. Use
     the daemon's `WriteBarrier` so the snapshot update commits
     atomically with finalisation. Daemon stays kind-agnostic:
     this is a generic "if the result envelope carries `cost`,
     write it to `budget`" rule, no kind matching.
  3. Acceptance: `mcp__rye__cli args=["thread","get","T-…"]`
     on a finalised thread returns a typed `usage` block with
     non-null fields when the runtime reported cost.

### O8. `PartialConfig` lacks `deny_unknown_fields` — SUBSUMED BY O1

The required-action list in O1 already covers this. Tracked here
only so a fresh LLM looking for "deny_unknown_fields" finds the
pointer.

### O9. Typed `ThreadUsage` slot on snapshot — SUBSUMED BY O3.A

O3.A step 1 already replaces `ThreadSnapshot.budget`'s
`Option<serde_json::Value>` with `Option<ThreadUsage>`. Listed
here only as a back-pointer.

### O10. Strict envelope-shape contract — LOW

- Affected structs (audit each for `#[serde(deny_unknown_fields)]`):
  - `ryeos-runtime/src/envelope.rs::LaunchEnvelope`
  - `ryeos-runtime/src/envelope.rs::EnvelopeRequest`
  - `ryeos-runtime/src/envelope.rs::EnvelopePolicy` and nested
    `HardLimits`
  - `ryeos-runtime/src/envelope.rs::EnvelopeResolution`
  - `ryeos-runtime/src/envelope.rs::EnvelopeRoots`
  - `ryeos-runtime/src/envelope.rs::EnvelopeCallback`
  - `ryeos-runtime/src/envelope.rs::EnvelopeInventory`
  - `ryeos-runtime/src/envelope.rs::RuntimeResult`
  - `ryeos-runtime/src/envelope.rs::RuntimeCost`
- Intentionally open-payload fields (DO NOT remove these — they
  are by-design `serde_json::Value` because the shape is
  kind-defined):
  - `EnvelopeRequest.inputs` — directive/graph inputs are
    user-defined per-kind.
  - `RuntimeResult.result` — the runtime's final output is
    kind-defined.
  - `RuntimeResult.outputs` — same.
- Required action: add `#[serde(deny_unknown_fields)]` to every
  closed-shape struct above. Leave the three open-payload fields
  alone but document them inline as "intentionally open".

### O11. UDS callback transport has no fallback — MEDIUM

- Files:
  - `ryeosd/src/uds/server.rs:68-105` (UDS RPC dispatch)
  - `ryeos-runtime/src/callback_client.rs:58-80` (UDS client)
  - `ryeos-runtime/src/callback_uds.rs`
  - `ryeosd/src/api/dispatch_launch.rs:1-6` (HTTP `/execute/stream`
    is unidirectional SSE, NOT a callback transport)
- If the daemon's UDS socket goes away mid-turn (daemon
  restart, crash, file-permission change), the runtime's
  resume-critical callbacks fail. There is NO HTTP fallback.
- Required action:
  1. Confirm explicitly in code: a runtime whose UDS callback
     fails on a resume-critical method (per O3.C) MUST exit
     with a typed `success: false` envelope. No retry loop, no
     in-memory queue.
  2. Test: kill the daemon mid-turn, assert the runtime's
     stdout envelope captures the typed callback failure;
     reconcile on next daemon boot finalize-as-failed.
- Out of scope here: a real HTTP fallback transport. That is
  advance-path work and the doc explicitly defers it.

### O12. UDS RPC request structs lack `deny_unknown_fields` — LOW

- Files:
  - `ryeosd/src/services/event_store.rs:14-49` (event-store RPC
    params)
  - `ryeosd/src/services/thread_lifecycle.rs:41-123` (thread
    lifecycle RPC params)
- Today these deserialise from JSON without strict serde, so a
  caller (CLI, runtime, or future remote) sending an unknown
  field gets silent acceptance.
- Required action: `#[serde(deny_unknown_fields)]` on every
  RPC param struct in `ryeosd/src/services/`. Audit by
  `rg "Deserialize" ryeosd/src/services/`.
- Acceptance: a request body with an unknown field returns a
  typed error from the RPC dispatcher.

### O13. `state_store` silent JSON decode losses — LOW

- File: `ryeosd/src/state_store.rs:287-308` (and any other site
  that does `serde_json::from_str(...).ok()` on persisted
  thread-result / artifact metadata).
- Decode failures here become `None`, which the read path treats
  as "no result" — semantically a silent data loss.
- Required action: replace each `.ok()` with a typed error path
  that surfaces the decode failure to the operator. Same
  classification as O5 / O6: corruption is fail-loud, not
  fail-quiet.

---

### O4a. Boot smoke pass — after O1/O2 ship

- Single goal: prove a fresh daemon boots cleanly with no stderr
  warnings against the standard bundle.
- Steps:
  1. `rm -rf /tmp/ryeosd-state-smoke/` (or whatever fresh
     `state_dir` you choose).
  2. Launch:
     `target/debug/ryeosd --init-if-missing --bind 127.0.0.1:7401
     --state-dir /tmp/ryeosd-state-smoke
     --system-data-dir ryeos-bundles/core`.
  3. Set `RYEOS_STATE_DIR` in `~/.config/amp/settings.json` →
     ask user to reload `rye` MCP server.
  4. `mcp__rye__cli args=["status"]` → expect exit 0 with live
     daemon info; ZERO `rye: warning:` lines on stderr.
  5. `mcp__rye__cli args=["help"]` → expect every verb listed,
     no warnings.

### O4b. Full 17-verb walk — after everything else lands

Confirmed-good 17 verbs after B2 deletion (each has a 1:1 daemon
handler under `ryeosd/src/services/handlers/`):

```
bundle install   bundle list      bundle remove
commands submit
events chain-replay   events replay
fetch
identity public-key
maintenance gc
rebuild
sign
status
thread chain   thread children   thread get   thread list
verify
```

Walk every verb via `mcp__rye__cli args=[…]`. For each verb,
record:
- the exact `args` invocation
- expected exit code (0)
- expected stdout JSON shape (key fields the verb returns)
- whether stderr is empty (it must be — any `rye: warning:`
  is a signature/trust regression, not a verb bug)

Any verb that returns surprising errors, missing fields, broken
JSON, or stderr warning lines becomes a numbered sub-issue under
O4b. Each surfaced issue gets a real fix in the -routes
worktree, NOT a workaround.

### O14. Operator runbook for foreign-schema bail — DOC

When B3 / O2's schema-ownership check fires, the daemon refuses
to boot. The operator needs an explicit recovery procedure:

1. Stop the daemon (it failed to start, so likely a no-op).
2. Identify the offending file from the typed error message —
   it points at the exact path (e.g. `~/.local/state/ryeosd/db/
   ryeosd.sqlite3` or `<state_dir>/.ai/state/projection.db`).
3. Archive (don't delete) by renaming with a timestamp:
   `mv ryeosd.sqlite3 ryeosd.sqlite3.foreign.$(date +%s)`. Same
   for projection.db.
4. Restart the daemon with `--init-if-missing`. The daemon
   re-creates the file from scratch; CAS objects are NOT
   touched (they're under `<state_dir>/.ai/state/objects/` and
   the schema-ownership check covers ONLY the SQLite cache
   files, not CAS).
5. If the operator needs to recover thread state from the
   archived file, that's a separate forensic task — the daemon
   never reads it again automatically.

Document this in the daemon's README or a dedicated runbook;
the typed bail message in `runtime_db.rs` and `projection.rs`
should link to it.

### O15. Deprecate stale advance-path docs — DOC

`docs/future/native-runtimes.md` and
`docs/future/RESUME-ADVANCED-PATH.md` no longer reflect the
shipped architecture and would mislead a fresh LLM that found
them via search. Required action:

1. Add a top-of-file banner to each:
   ```
   > **DEPRECATED 2026-04-28** — this document is preserved for
   > history but does NOT reflect current architecture. Do not
   > anchor implementation on its contents. See
   > `docs/future/mcp-end-to-end-bug-sweep.md` for current
   > forward planning.
   ```
2. Or delete them outright if no historical value. Decide with
   the user.

---

## Operational rules for the executing agent

- **Daemon stays kind-agnostic and vendor-agnostic** at all costs.
  No `match kind_name { "directive" => … }` in `ryeosd/`. If a
  fix needs runtime-specific behaviour, push it into the runtime
  crate (e.g. `ryeos-directive-runtime`, `ryeos-graph-runtime`) so
  the daemon dispatches uniformly. O3.E demonstrates the
  pattern: the safety gate lives in the runtime, the daemon sees
  only a typed `RuntimeResult` failure.
- Cargo binary at `/home/leo/.local/share/cargo/bin/cargo` (NOT in
  PATH).
- Re-sign YAMLs ONLY via
  `cargo run --example resign_yaml -p ryeos-engine -- <abs-path>`.
  Never `mcp__rye__sign` for this kind of work.
- Re-build the standard bundle MANIFEST only via
  `cargo run -q -p ryeos-cli --bin rye-bundle-tool -- rebuild-manifest
  --source ryeos-bundles/standard --seed 119` — only needed if
  `bin/<triple>/*` binaries change. Pure config/cli/*.yaml edits do
  not require it.
- Per-change verification gate (must pass):
  - `cargo build --workspace --all-targets 2>&1 | grep -iE
    "warning|error"` empty.
  - `cargo test -p ryeosd --no-fail-fast` all green, floor 440 (B3
    raised it from 438).
  - For YAML edits: re-sign, then `mcp__rye__cli args=["help"]`
    shows the verb without warning lines on stderr.
- Commit format when authorised: subject + `Amp-Thread-ID: <id>` +
  `Co-authored-by:` lines per existing commits 605b1f0a / 0b0aa5e8.
  Do NOT commit unless explicitly told to. Pause for review at every
  commit boundary.
- Disk: /home periodically saturates. `cargo clean -p ryeosd && rm
  -rf target/debug/incremental` frees ~15 GB on this host.
- Pre-existing flaky test (do NOT touch):
  `ryeos-graph-runtime hooks::tests::fire_hook_emits_span` fails
  under workspace concurrent load, passes in isolation.

---

## What "done" looks like

- O1 fixed and tested: fresh init `--bind` writes CLI value;
  conflicting `--bind` against existing config errors loudly;
  matching `--bind` accepted; malformed `config.yaml` refuses to
  start; `PartialConfig` rejects unknown fields.
- O2 fixed and tested: every daemon-owned SQLite file
  (`runtime.db` and `projection.db`) verified by exact
  `SchemaSpec` + `application_id` before any DDL touches it; both
  refuse foreign tables, refuse missing expected columns, and
  correctly ignore `sqlite_*` internals + autoindexes; both run
  fresh-init cleanly.
- O3.A–E delivered: typed `ThreadUsage` round-tripped through the
  CAS, resume reseeds both `BudgetTracker` and `Harness`, the
  legacy `type`/`data` event reader replaced with typed
  cross-crate event shape, resume-critical callbacks fail loud
  on disconnect, durable settlement (event append + snapshot
  update) precedes both ACK and live broadcast. Runtime-side
  resume gate (O3.E) self-removes when prerequisites are met.
- O5–O7, O11–O13 done: no schema-mismatch warn-and-continue path,
  resume-attempts corruption bails, daemon round-trips
  `RuntimeCost` at BOTH caller envelope and durable
  finalisation layers, UDS callback failures surface as typed
  `success: false` envelopes, every UDS RPC param struct uses
  `#[serde(deny_unknown_fields)]`, every `state_store.rs`
  `serde_json::from_str(...).ok()` replaced with a typed error.
- O10 done: every closed-shape struct in
  `ryeos-runtime/src/envelope.rs` carries
  `#[serde(deny_unknown_fields)]`; the three intentionally-open
  payload fields (`EnvelopeRequest.inputs`,
  `RuntimeResult.result`, `RuntimeResult.outputs`) documented
  inline.
- O14: typed bail messages in `runtime_db.rs` and
  `projection.rs` link to a documented operator recovery
  runbook.
- O15: stale `native-runtimes.md` and `RESUME-ADVANCED-PATH.md`
  carry deprecation banners or are deleted (per user choice).
- O4a: fresh daemon boots clean, `mcp__rye__cli args=["status"]`
  returns live info with zero `rye: warning:` lines on stderr.
- O4b: all 17 verbs walk cleanly; every surfaced sub-issue has
  a real fix.

### Concrete test obligations

**`ryeosd`** (current floor 440 after B3; realistic post-fix
floor ≈ 452–456):

1. Fresh init writes `--bind` value to `config.yaml`.
2. Existing config + conflicting `--bind` errors.
3. Existing config + matching `--bind` succeeds.
4. Malformed `config.yaml` refuses to start.
5. `PartialConfig` rejects an unknown top-level key.
6. `RuntimeDb::open` rejects an extra unexpected user table.
7. `RuntimeDb::open` accepts SQLite internal objects /
   autoindexes (negative test for the false-reject hazard).
8. Corrupt `launch_metadata` row errors (replaces today's
   `garbage_launch_metadata_decodes_to_none_without_panic`).
9. `launch_metadata` schema-version mismatch errors (replaces
   today's `schema_version_mismatch_yields_none_with_warn`).
10. `get_resume_attempts` on a missing row returns 0.
11. `get_resume_attempts` on a NULL `resume_attempts` errors.
12. Reconcile finalises-as-failed when the resume_attempts read
    errors.
13. Launch result envelope serialises a typed `cost` block.
14. Daemon-side `thread_usage` callback persists the snapshot
    AND appends the event before ACKing the callback.
15. Daemon-side `thread_usage` live broadcast fires only after
    durable settlement (persistence-first invariant).
16. `thread get` daemon read path surfaces typed `usage`.
17. UDS RPC param struct rejects unknown field.

**`ryeos-state`**:

1. Projection DB rejects foreign table set.
2. Projection DB rejects missing expected column.
3. Projection DB rejects extra unexpected user table.
4. Projection DB ignores SQLite internals / autoindexes.
5. Typed `ThreadUsage` snapshot serde round-trip.
6. Snapshot finalisation populates `budget` from
   `RuntimeCost`.

**`ryeos-directive-runtime`**:

1. Resume load rejects missing `events`.
2. Resume load rejects unknown event-type variant.
3. Resume reseeds `BudgetTracker` from prior `ThreadUsage`.
4. Resume reseeds `Harness` from prior `ThreadUsage`.
5. Over-budget thread refuses to start on resume.
6. Resume-critical callback disconnect errors with typed
   `success: false` envelope (no warn-and-continue).
7. Turn settlement persists `thread_usage` durably BEFORE
   updating local `Harness` / `BudgetTracker`.
8. Runtime-side resume refusal (O3.E) returns
   `Ok(RuntimeResult { success: false, … })` with exit code 0
   when prerequisites are unmet.

**`ryeos-runtime`**:

1. Every closed-shape envelope struct rejects an unknown field.
2. The three intentionally-open Value fields accept arbitrary
   JSON.

No `#[ignore]`, no FIXMEs, no `#[allow(dead_code)]` in
route-system code.
