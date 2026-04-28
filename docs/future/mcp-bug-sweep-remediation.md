# MCP bug-sweep remediation — close the gaps oracle flagged

Status: handoff doc. Companion to
`docs/future/mcp-end-to-end-bug-sweep.md`.
Worktree: `/home/leo/projects/ryeos-cas-as-truth` (branch
`feat/cas-as-truth-rye-state`). Build is currently clean and
695 tests pass, but oracle review found that 11 of 13 "done"
issues are not actually closed.

This doc enumerates the concrete remaining work to actually
finish O1–O15. The first sweep ([../future/mcp-end-to-end-bug-sweep.md])
remains the source of truth for design intent; this doc is the
delta from "claimed done" to "really done", with the exact
file:line citations from the oracle pass.

Hard project rules (unchanged):
NO backwards compatibility, NO legacy refs, NO silent fallbacks,
NO migration shims, NO `#[ignore]`, NO FIXMEs, NO
`#[allow(dead_code)]` in route-system code, typed body
deserialization with `#[serde(deny_unknown_fields)]`, typed
fail-loud everywhere. Daemon stays kind-agnostic and
vendor-agnostic. Persistence-first invariant: live broadcasts
NEVER deliver an event the durable store doesn't already hold.

---

## Remediation execution order

These are the gaps you must close. Land in this order to
respect dependencies.

1. **R1** — fix `--bind` precedence (O1 regression introduced
   by partial implementation; daemon now incorrectly errors on
   normal startup).
2. **R2** — actually verify schema ownership (O2 didn't use
   the index columns/uniqueness it stamped).
3. **R3** — daemon-side `thread_usage` settlement path (O3.A
   hardest gap; everything in O3.D and O7-layer-2 depends on
   it).
4. **R4** — runner-side resume-critical fail-loud (O3.C wrap
   removal + O3.D ordering follows from R3).
5. **R5** — runtime-side resume gate (O3.E cleanup once R3
   makes prerequisites checkable).
6. **R6** — finalize-callback round-trips real cost (O7 layer
   2; no fabricated placeholders).
7. **R7** — kill remaining silent fallbacks (`extract_thread_usage`,
   `resume_attempts` clamp, `cost` facet parse, artifact
   publish fabrication).
8. **R8** — finish typed-shape audit (O10 imports, O12
   command_service).
9. **R9** — kill-daemon-mid-turn integration test (O11
   acceptance).
10. **R10** — schema-bail error path + operator-runbook README
    (O14 + O15 stragglers).

---

## R1. Fix `--bind` precedence — HIGH

**What's wrong**

`ryeosd/src/config.rs:90-101, 109-112, 121-132, 146-149`,
`ryeosd/src/bootstrap.rs:28-31, 155-161`. The conflict check
compares `file_bind` to `cli.bind` without knowing whether
`--bind` was actually supplied. Clap's default makes `cli.bind`
always populated, so any `config.yaml` with a non-default
`bind` value now fails normal startup. Worse, `--force` does
NOT overwrite the file value because the final selection
`config.rs:146-149` still prefers file-side.

**Required action**

1. Make `Cli.bind: Option<String>` (NOT default-valued). When
   the operator omits `--bind`, the field is `None`.
2. Conflict logic in `config.rs:121-132`:
   - `(file_bind=None, cli=None)` → use compiled-in default.
   - `(file_bind=Some(x), cli=None)` → use file value, no
     error.
   - `(file_bind=None, cli=Some(x))` → use cli value (this is
     a fresh-init or unconfigured-bind case), persist on init.
   - `(file_bind=Some(x), cli=Some(y))` AND `x == y` → use it.
   - `(file_bind=Some(x), cli=Some(y))` AND `x != y` AND
     `--force` not set → typed bail.
   - `(file_bind=Some(x), cli=Some(y))` AND `x != y` AND
     `--force` → use cli value AND rewrite `config.yaml` so
     subsequent boots are consistent.
3. `bootstrap::init` (`bootstrap.rs:28-31`, `155-161`) must
   honour the rewrite-on-`--force` path. The current "only
   writes when missing or force" rule is correct; just feed it
   the resolved bind, not the file's stored bind.

**Acceptance / tests**

Replace any existing tests:
- `Cli` parses `--bind 127.0.0.1:7401` to `Some(...)`.
- No `--bind` parses to `None`.
- Fresh init + `--bind` writes that exact value to
  `config.yaml`.
- Existing config + no `--bind` boots clean.
- Existing config + matching `--bind` boots clean.
- Existing config + conflicting `--bind` (no `--force`) errors
  loudly with the typed message.
- Existing config + conflicting `--bind` + `--force` rewrites
  `config.yaml` to the cli value.
- Malformed `config.yaml` refuses to start.

---

## R2. Make schema-ownership actually exact — HIGH

**What's wrong**

`ryeos-state/src/sqlite_schema.rs:75-76, 208-250`. The doc
comment promises `PRAGMA index_list` + `PRAGMA index_info`
verification of every index's table, columns, and uniqueness.
The implementation only checks index *names* and rejects
unknown ones. `IndexSpec.columns` and `IndexSpec.unique` at
`sqlite_schema.rs:48-55` are dead. A foreign DB with the same
index names but different shape passes today.

**Required action**

1. For each `IndexSpec` in the spec, run
   `PRAGMA index_list(<table>)` to find the matching index by
   name, assert `unique` flag matches `IndexSpec.unique`.
2. Run `PRAGMA index_info(<index_name>)` and assert ordered
   column list matches `IndexSpec.columns`.
3. Bail with a typed error citing `<table>.<index> expected
   columns [a,b] unique=true; got [a] unique=false` when any
   piece deviates.
4. Reject any user-owned index name not in the spec, same as
   today (the autoindex/`sqlite_*` filter at
   `sqlite_schema.rs:222-226` stays).
5. Errors must carry the offending file path (see R10) — pass
   `path: &Path` into `assert_owned`.

**Acceptance / tests**

In `ryeos-state/src/sqlite_schema.rs` tests + projection +
runtime_db tests:
- Foreign DB whose index has the right name but wrong column
  set fails.
- Foreign DB whose index has matching shape but wrong
  uniqueness fails.
- Owned DB with the exact spec passes idempotently across
  multiple opens.

---

## R3. Daemon-side `thread_usage` settlement path — CRITICAL

This is the largest gap and unblocks R4–R6.

**What's wrong**

The runtime emits `thread_usage` callbacks
(`ryeos-runtime/src/callback_client.rs:312-328`,
`ryeos-directive-runtime/src/runner.rs:270-274`) but the daemon
has no handler for them. `ryeosd/src/state_store.rs:1043-1059`
is generic event append; it never updates
`ThreadSnapshot.budget`. Resume reads via
`extract_thread_usage` (`ryeos-directive-runtime/src/resume.rs:36-50`)
look at a `"budget"` top-level key on the response of
`runtime.get_thread`, but `ThreadDetail`
(`ryeosd/src/state_store.rs:143-160`) has no usage field and
the UDS path (`ryeosd/src/uds/server.rs:173-178`) doesn't
return one. End result: `ThreadUsage` exists as a type, but no
production code actually round-trips it.

**Required action**

1. Add a `thread_usage` UDS RPC verb in
   `ryeosd/src/uds/server.rs` — same wire shape as the existing
   `event_append` verbs, params struct
   `#[serde(deny_unknown_fields)]` carrying `thread_id` +
   typed `ThreadUsage`.
2. New `StateStore::settle_thread_usage(thread_id, usage:
   ThreadUsage)` method that, under
   `ryeosd::write_barrier::WriteBarrier`, atomically:
   - appends a typed `thread_usage` event to the durable
     event log,
   - reads the current `ThreadSnapshot`, merges
     `usage` into `snapshot.budget`,
   - writes the new `ThreadSnapshot`,
   - updates the projection (`ryeos-state` projection write
     path),
   - returns Ok only if all three commit.
3. Daemon ACKs the callback ONLY after that returns Ok.
4. Live broadcast of the `thread_usage` event happens AFTER
   ACK (persistence-first; matches the existing append-publish
   ordering at `uds/server.rs:195-205, 208-229`).
5. Extend `ThreadDetail` (`state_store.rs:143-160`) with
   `usage: Option<ThreadUsage>` populated from the snapshot
   read.
6. `uds/server.rs::handle_thread_get` (`173-178`) returns the
   new field.
7. `ryeos-runtime/src/callback_client.rs::get_thread_by_id`
   becomes resume-critical (it's currently advisory at
   `228-234`); on disconnect it errors. The `replay_events_for`
   classification stays correct.
8. `ryeos-directive-runtime/src/resume.rs::extract_thread_usage`
   is rewritten — no `unwrap_or(None)` swallowing — to
   deserialise the typed `usage` field directly off
   `ThreadDetail`. Missing `usage` AND
   `previous_thread_id.is_some()` is a fail-loud condition for
   R5.

**Daemon stays kind-agnostic.** The `thread_usage` handler is
keyed on event-type, not on `kind_name`. Any runtime that
emits `thread_usage` gets identical settlement.

**Acceptance / tests**

- `ryeosd` integration test: post a `thread_usage` UDS RPC,
  assert the event log has the event AND the snapshot
  `budget` was updated AND the live broadcast fires after
  both. Crash-injection between durable-write and ACK must
  not yield a published event with no snapshot update.
- `ryeos-directive-runtime` test: a thread with a prior
  `thread_usage` settled on the daemon, on resume the runtime
  reads typed `ThreadDetail.usage` and reseeds
  `BudgetTracker` + `Harness`.

---

## R4. Runner-side resume-critical fail-loud — HIGH

**What's wrong**

Despite client-side hard-fails landing
(`ryeos-runtime/src/callback_client.rs:108-119, 173-182,
186-195, 238-248, 312-328`), the directive runtime wraps every
call in `record_callback_warning`
(`ryeos-directive-runtime/src/runner.rs:83-94`) used at
`201-205, 232-236, 270-274, 282-290, 527-530, 671-675,
706-709`. Errors become log lines and the run continues.
Classification comments at
`callback_client.rs:262-296` say methods like
`emit_turn_complete` and `emit_tool_*` are "Advisory" — but
they call `append_event` which hard-fails on
transcript-bearing event types. Comments contradict behaviour.

**Required action**

1. Walk every call site of `record_callback_warning` in
   `runner.rs`. For each, classify the call as
   resume-critical or advisory using the criteria from
   `mcp-end-to-end-bug-sweep.md` O3.C.
2. Resume-critical sites drop `record_callback_warning` and
   propagate the error. The runner state machine transitions
   to a terminal `Errored { error: ... }` state with a typed
   reason and emits
   `Ok(RuntimeResult { success: false, status: "errored",
   result: Some(...) })`.
3. Specifically: `mark_running`, `emit_thread_usage`,
   `finalize_thread`, the transcript-bearing `append_event`s
   (`cognition_in`, `cognition_out`, `tool_dispatch`,
   `tool_result`) → resume-critical → drop the warning wrap.
4. `stream_opened`, progress-only `emit_turn_start` →
   advisory → keep `record_callback_warning`.
5. Update the doc comments above each method in
   `callback_client.rs:262-296` so comment matches behaviour.
6. After R3 lands, `emit_thread_usage` IS the durable
   settlement; if it errors, the runner aborts BEFORE any
   in-process Harness/Budget mutation. This delivers O3.D's
   ordering contract for real.

**Acceptance / tests**

- A directive-runtime test that simulates a UDS disconnect
  during `emit_thread_usage`. The runner must exit with
  `success: false` and the in-process budget must NOT have
  been updated.
- Same shape for `mark_running`, `finalize_thread`, each
  transcript-bearing `append_event`.

---

## R5. Runtime-side resume gate — MEDIUM

**What's wrong**

`ryeos-directive-runtime/src/main.rs:134-158` has no "resume
prerequisites unmet" path; it just enters `Runner::from_resume`
unconditionally when `previous_thread_id.is_some()`. After R3,
the prerequisites are checkable.

**Required action**

1. In `run_with_envelope`, when
   `envelope.request.previous_thread_id.is_some()`:
   - Fetch `ThreadDetail` for the previous thread.
   - If `detail.usage.is_none()` OR the typed event log fails
     to deserialise (per O3.B contract), return
     `Ok(RuntimeResult { success: false, status: "errored",
     result: Some("resume prerequisites unmet: <typed
     reason>"), … })` with exit code 0.
2. Once R3 ships, this gate self-disarms because
   prerequisites become routinely satisfied. Leave the gate
   in place permanently — it's the runtime's own
   pre-condition check, not a temporary shim.

**Acceptance / tests**

- Synthetic resume against a daemon that has no `usage`
  recorded yields `Ok(RuntimeResult{success:false})` exit 0.
- Real-flow resume (R3 settled a prior turn) succeeds.

---

## R6. Finalize callback carries real cost — HIGH

**What's wrong**

`ryeos-directive-runtime/src/runner.rs:443-446, 671-675,
706-709` calls finalize with only `status`.
`ryeos-runtime/src/callback_uds.rs:101-112` ships only
`thread_id` + `status`. The daemon's `ThreadFinalizeParams`
(`ryeosd/src/services/thread_lifecycle.rs:105-121`) accepts
`final_cost`; that's bypassed. Worse,
`state_store.rs:519-540` fabricates
`started_at=created_at`, `last_settled_turn_seq=0`,
`elapsed_ms=0` from facets — durable-truth-by-fabrication.

**Required action**

1. Extend `CallbackClient::finalize_thread` and the
   underlying UDS request struct to carry an
   `Option<ThreadUsage>`.
2. Runner finalize callsites
   (`runner.rs:443-446, 671-675, 706-709`) construct the
   typed `ThreadUsage` from the in-process `Harness` /
   `BudgetTracker` (which by R3+R4 have been kept in sync
   with durable settlement) and pass it.
3. Daemon `ThreadFinalizeParams.final_cost` becomes
   `Option<ThreadUsage>` and `state_store.rs:519-540` writes
   it directly into `ThreadSnapshot.budget` — no fabrication
   from facets. Drop the placeholder fields entirely.
4. The caller-facing launch envelope already carries `cost`
   (`ryeosd/src/execution/launch.rs:332-338`). Confirm it
   reads from the same finalised `ThreadSnapshot.budget` so
   one source of truth feeds both layers.

**Acceptance / tests**

- A directive run completes; `mcp__rye__cli args=
  ["thread","get","T-…"]` returns the typed `usage` block
  with non-fabricated `started_at`, `last_settled_turn_seq`,
  `elapsed_ms` populated from runtime data.
- The caller-facing launch envelope `cost` matches the
  durable `ThreadSnapshot.budget`.

---

## R7. Kill remaining silent fallbacks — MEDIUM

Concrete sites the oracle flagged that are NOT covered by R1–R6:

1. `ryeos-directive-runtime/src/resume.rs:36-50`
   `extract_thread_usage` swallows every error path. Rewrite
   to typed deserialisation as part of R3 step 8.
2. `ryeosd/src/runtime_db.rs:260` — negative `resume_attempts`
   silently clamps to 0 via `.max(0) as u32`. Replace with
   `try_from` and bail on negative; the column should never
   carry a negative anyway.
3. `ryeosd/src/state_store.rs:532-536` — cost-facet parse
   falls back to 0. After R6 the cost no longer comes from
   facets; remove the fallback altogether.
4. `ryeosd/src/state_store.rs:841-865` — artifact-publish
   fabricates fallback persisted data. Replace with typed
   bail when the read returns None or decode fails.
5. `ryeos-directive-runtime/src/resume.rs:69-78` — tool-call
   payload walk uses `filter_map`, `unwrap_or("")`,
   `unwrap_or(Value::Null)`. Replace each with typed
   deserialisation; malformed payload → bail.

**Acceptance / tests**

- Negative `resume_attempts` errors loudly.
- Missing `usage` errors loudly (R3).
- Malformed tool-call payload errors loudly.
- Artifact-publish on missing source data errors loudly.

---

## R8. Finish typed-shape audit — LOW

**What's wrong**

- `LaunchEnvelope.resolution` (`ResolutionOutput` at
  `ryeos-engine/src/resolution/types.rs:200-219`) and
  `LaunchEnvelope.inventory[*]` (`ItemDescriptor` at
  `ryeos-engine/src/inventory.rs:71-80`) lack
  `#[serde(deny_unknown_fields)]`.
- `ryeosd/src/services/command_service.rs:18-26, 28-33,
  40-46` — `CommandSubmitParams`, `CommandClaimParams`,
  `CommandCompleteParams` not strict.

**Required action**

Add `#[serde(deny_unknown_fields)]` to every closed-shape
struct above. Confirm the open-payload fields documented in
the original sweep (`EnvelopeRequest.inputs`,
`RuntimeResult.result`, `RuntimeResult.outputs`) remain
intentionally open.

**Acceptance / tests**

For each newly-strict struct, a test that asserts an unknown
field is rejected.

---

## R9. Kill-daemon-mid-turn integration test — MEDIUM

**What's wrong**

O11 was claimed done but the required end-to-end test does
not exist.

**Required action**

In `ryeosd` integration tests, a test that:
1. Starts the daemon with a fresh state dir.
2. Launches a multi-turn directive that emits one
   `thread_usage` callback successfully, then the test
   harness kills the daemon mid-second-turn.
3. On daemon restart with `--init-if-missing`, reconcile
   finalises the thread as failed.
4. The runtime's stdout envelope (captured by the harness)
   shows `success: false` with a typed callback-failure
   reason.
5. The thread's durable `ThreadSnapshot.budget` reflects ONLY
   the first settled turn — not the unsettled second.

**Acceptance**

That test passes deterministically.

---

## R10. Schema bail messages + operator runbook — LOW

**What's wrong**

- `ryeos-state/src/sqlite_schema.rs:87-90, 124-127, 153-156,
  244-247` use `<file>` placeholders.
- Callers `ryeosd/src/runtime_db.rs:134-139` and
  `ryeos-state/src/projection.rs:284-290` don't pass the path
  in.
- O14 asked for a README/runbook; only an in-source comment
  exists.
- O15 deleted stale docs but
  `docs/future/knowledge-runtime.md:25`,
  `resolution-pipeline-advanced.md:191`,
  `knowledge-runtime-arc-agi-3-design.md:500` still link to
  the deleted files.

**Required action**

1. `assert_owned(conn, spec, path)` takes a `&Path` and
   formats it into every error message. Same for
   `init_owned`.
2. Update the two callers to pass their path.
3. Write
   `docs/operations/foreign-schema-recovery.md` (NEW). Pull
   the recovery procedure out of
   `mcp-end-to-end-bug-sweep.md` O14 verbatim. Have every
   typed schema-bail link to it.
4. Grep for references to the deleted
   `native-runtimes.md` / `RESUME-ADVANCED-PATH.md` and
   either remove the references or update the linked text to
   point at `mcp-end-to-end-bug-sweep.md` /
   `mcp-bug-sweep-remediation.md`.

**Acceptance**

- A schema-bail error string contains the actual file path.
- The runbook exists at the linked location and is reachable
  from the bail messages.
- `rg "native-runtimes\.md|RESUME-ADVANCED-PATH\.md" docs/`
  returns nothing (or only the new remediation doc).

---

## Operational rules for the executing agent

(Same as the original sweep; restated because this is the
handoff a fresh LLM may pick up.)

- **Daemon stays kind-agnostic.** Any required action that
  forces `match kind_name` in `ryeosd/` is a hard violation.
  R3's `thread_usage` handler keys on event type.
- Cargo binary at `/home/leo/.local/share/cargo/bin/cargo`
  (NOT in PATH).
- Re-sign YAMLs ONLY via
  `cargo run --example resign_yaml -p ryeos-engine -- <abs-path>`.
- Per-change verification gate (must pass):
  - `cargo build --workspace --all-targets 2>&1 | grep -iE
    "warning|error"` empty.
  - `cargo test -p ryeosd -p ryeos-state -p ryeos-runtime
    -p ryeos-directive-runtime --no-fail-fast` all green.
    Floor moves UP per remediation; do not regress.
- Pre-existing flaky test (do NOT touch):
  `ryeos-graph-runtime hooks::tests::fire_hook_emits_span`.
- Commit format when authorised: subject +
  `Amp-Thread-ID: <id>` + `Co-authored-by:` lines per
  existing commits. Pause for review at every commit
  boundary. Do not bundle.

---

## What "remediation done" looks like

- R1: `--bind` precedence works in all five branches; no
  false-error on stored-non-default bind; `--force` rewrites.
- R2: a foreign DB whose indexes have the right names but
  wrong shape fails the schema-ownership check.
- R3: `thread_usage` is durably settled atomically with
  snapshot update; resume reads typed `ThreadDetail.usage`.
- R4: every resume-critical callsite hard-fails on disconnect;
  in-process Harness/Budget never advances past a failed
  durable settlement; comments match behaviour.
- R5: runtime self-refuses resume on missing prerequisites.
- R6: finalize-callback carries typed `ThreadUsage`; no
  fabricated placeholder fields anywhere in the durable path.
- R7: zero `unwrap_or` / `.ok()` / `filter_map` on
  resume- or persistence-critical paths.
- R8: `LaunchEnvelope` is fully strict; `command_service`
  RPC params strict.
- R9: kill-daemon-mid-turn test passes deterministically.
- R10: schema bails carry real paths and link to a real
  runbook; no dangling references to the deleted advance
  paths.

After all ten: the "done" criteria of
`mcp-end-to-end-bug-sweep.md` are actually met.
