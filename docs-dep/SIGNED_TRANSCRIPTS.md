# Signed Transcripts & JSON Signing — Checkpoint Integrity for Thread Execution Records

> **Extends:** [`THREAD_SYSTEM_PART2_ADVANCED_ORCHESTRATION.md`](THREAD_SYSTEM_PART2_ADVANCED_ORCHESTRATION.md)
> **Related:** [`STATE_GRAPH_AND_PROGRAMMATIC_EXECUTION.md`](STATE_GRAPH_AND_PROGRAMMATIC_EXECUTION.md) (same signed-knowledge pattern applied to graph state)

---

## 1. The Problem

Thread transcripts are unsigned. `transcript.jsonl` is the authoritative record of what an LLM said and did — and it's a plain text file anyone can edit.

This matters at two trust boundaries:

1. **Resume** — `orchestrator.resume_thread()` calls `transcript.reconstruct_messages()` which reads `.jsonl` line by line and rebuilds conversation messages. The resumed thread operates on this reconstructed context. If someone inserted a fake tool result, deleted a safety refusal, or modified an LLM response between failure and resume, the new thread acts on falsified history.

2. **Handoff** — `orchestrator.handoff_thread()` reads `transcript.md` and feeds it to the summary directive. A tampered transcript produces a tampered summary, which becomes the foundation for the continuation thread's context.

Neither path verifies integrity. Both trust the filesystem blindly.

Thread metadata (`thread.json`) is also unsigned. It contains security-sensitive fields — `capabilities` and `limits`. If a child thread or graph walker reads parent capabilities from `thread.json`, a tampered file could escalate permissions.

### What's NOT the problem

- **During execution** — the runner writes events and reads them back within the same process. No trust boundary. Signing during execution is about audit trail, not active security.

### The broader gap

Rye's signing infrastructure covers markdown (directives, knowledge), Python, and YAML via embedded comment headers. Two file formats have no signing support:

| Format    | Used By            | Why No Signing Today                                  |
| --------- | ------------------ | ----------------------------------------------------- |
| **JSONL** | `transcript.jsonl` | No comment syntax, append-only (can't prepend header) |
| **JSON**  | `thread.json`      | No comment syntax                                     |

This doc solves both.

---

## 2. Design: Checkpoint Signing

Sign the transcript at turn boundaries, not after every event. A "turn" is one complete LLM cycle: user message → LLM response → tool calls → tool results. This is the natural checkpoint — it's where the conversation is in a consistent state.

### Why not sign every event?

The transcript is append-only. Each `write_event()` appends a JSON line. Signing after every event means re-hashing the entire file each time — the signature covers all content, not just the new line. For a 200-turn thread with 5 tool calls per turn, that's ~1000 events × ~1000 re-hashes = O(n²) hashing. Checkpoint signing at turn boundaries is ~200 signatures for the same thread.

### Why not sign only at suspend/resume?

That's Option C from the graph state analysis — minimal overhead but gaps in the audit trail. Between turn 1 and turn 199, there's no integrity record. If the thread completes normally (no resume), there's never a signature at all. Checkpoint signing gives you a verifiable record of the full execution, useful for audit, compliance, and debugging.

### The checkpoint: `cognition_in` of the next turn

The natural checkpoint is when the runner is about to send the next user message (i.e., the assembled tool results). At this point, the previous turn is fully committed to the transcript:

```
cognition_in (user prompt)         ← turn 1 starts
cognition_out (LLM response)
tool_call_start (tool A)
tool_call_result (tool A result)
tool_call_start (tool B)
tool_call_result (tool B result)
cognition_in (tool results)        ← turn 2 starts, SIGN HERE (covers turn 1)
cognition_out (LLM response)
...
thread_completed                   ← SIGN HERE (covers final turn)
```

Signing at the start of each turn means the signature covers all events up to that point. The final signature (at `thread_completed` or `thread_error`) covers the entire transcript.

---

## 3. JSONL Signing: Checkpoint as Event

### The insight

The transcript is already a typed event stream. `reconstruct_messages()` only processes `cognition_in`, `cognition_out`, `tool_call_start`, and `tool_call_result` — it skips everything else. A checkpoint signature is just another event type:

```jsonl
{"timestamp": 1739820405, "thread_id": "my-thread", "event_type": "cognition_out", "payload": {"text": "I'll analyze..."}}
{"timestamp": 1739820406, "thread_id": "my-thread", "event_type": "tool_call_result", "payload": {"call_id": "tc_1", "output": "..."}}
{"timestamp": 1739820406, "thread_id": "my-thread", "event_type": "checkpoint", "payload": {"turn": 1, "byte_offset": 2847, "hash": "a3f2...", "sig": "base64url...", "fp": "440443d0..."}}
{"timestamp": 1739820407, "thread_id": "my-thread", "event_type": "cognition_in", "payload": {"text": "...", "role": "user"}}
```

No sidecar file. No new file format. No changes to existing code paths:

- `reconstruct_messages()` — already skips unknown event types. `checkpoint` events are invisible.
- `_render_event()` — already returns `""` for unknown event types. `transcript.md` is unaffected.
- `write_event()` — checkpoint events are written via the same append mechanism as any other event.
- `EventEmitter` — checkpoint events are critical (synchronous write, same as `thread_completed`).

### What gets signed

The `byte_offset` in the checkpoint payload is the byte position of the first byte of the checkpoint line itself. The hash covers `transcript.jsonl[0:byte_offset]` — all content _before_ this checkpoint line. This is the same pattern as `# rye:signed:...` headers in Python files — the signature line is excluded from the content hash (via `extract_content_for_hash()` in `ToolMetadataStrategy`).

```
bytes 0-2846:    events from turn 1
byte 2847:       start of checkpoint line → hash covers [0:2847)
bytes 2847-2950: checkpoint event itself (not covered by its own hash)
bytes 2951+:     events from turn 2...
```

### No chicken-and-egg problem

The checkpoint event can't cover itself — but it doesn't need to. Each checkpoint covers all preceding content. The _next_ checkpoint covers the previous checkpoint event. The final checkpoint (at `thread_completed`) covers everything including all previous checkpoints. The only uncovered line is the final checkpoint itself — and that line is the signature, so self-coverage is meaningless.

### Why not use KnowledgeMetadataStrategy

The existing signing infrastructure embeds signatures at the top of files (`<!-- rye:signed:... -->` for markdown, `# rye:signed:...` for Python/YAML). This requires prepending — impossible for append-only files without a full rewrite. Checkpoint-as-event preserves the append-only pattern naturally.

The cryptographic primitives are the same — `ensure_keypair()`, `sign_hash()`, `compute_key_fingerprint()` from `lilux/primitives/signing.py`. Only the embedding format differs.

---

## 4. Implementation: TranscriptSigner

A new class that lives alongside `Transcript`. Writes checkpoint events directly into the JSONL stream — no sidecar files.

```python
# persistence/transcript_signer.py

class TranscriptSigner:
    """Checkpoint signing for transcript integrity.

    Signs transcript.jsonl at turn boundaries by appending a checkpoint
    event to the JSONL stream. Each checkpoint's hash covers all bytes
    before the checkpoint line (byte_offset = start of checkpoint line).

    Verification reads the JSONL, extracts checkpoint events, and verifies
    each hash + signature against the file content.
    """

    def __init__(self, thread_id: str, thread_dir: Path):
        self._thread_id = thread_id
        self._jsonl_path = thread_dir / "transcript.jsonl"

    def checkpoint(self, turn: int) -> None:
        """Sign the transcript up to its current size.

        Called by runner.py at turn boundaries (pre-turn, after
        all previous tool results are committed). The checkpoint
        event is appended to the same JSONL file as all other events.
        """
        if not self._jsonl_path.exists():
            return

        # byte_offset = current file size = start of the checkpoint line we're about to write
        byte_offset = self._jsonl_path.stat().st_size
        content = self._jsonl_path.read_bytes()
        content_hash = hashlib.sha256(content).hexdigest()
        timestamp = time.time()

        ed25519_sig = sign_hash(content_hash, private_pem)
        pubkey_fp = compute_key_fingerprint(public_pem)

        # Write as a standard JSONL event — same format as every other event
        entry = {
            "timestamp": timestamp,
            "thread_id": self._thread_id,
            "event_type": "checkpoint",
            "payload": {
                "turn": turn,
                "byte_offset": byte_offset,
                "hash": content_hash,
                "sig": ed25519_sig,
                "fp": pubkey_fp,
            },
        }
        with open(self._jsonl_path, "a") as f:
            f.write(json.dumps(entry, default=str) + "\n")
            f.flush()

    def verify(self) -> Dict:
        """Verify the transcript against its checkpoint events.

        Reads the JSONL, extracts checkpoint events, and verifies each
        hash + Ed25519 signature against the file content at that byte offset.

        Returns:
            {"valid": True, "checkpoints": N} on success.
            {"valid": False, "error": "...", "failed_at_turn": N} on failure.
        """
        if not self._jsonl_path.exists():
            return {"valid": True, "checkpoints": 0, "unsigned": True}

        content = self._jsonl_path.read_bytes()

        # Extract checkpoint events from the JSONL stream
        checkpoints = []
        for line in content.decode("utf-8").splitlines():
            line = line.strip()
            if not line:
                continue
            try:
                event = json.loads(line)
                if event.get("event_type") == "checkpoint":
                    checkpoints.append(event["payload"])
            except json.JSONDecodeError:
                continue

        if not checkpoints:
            return {"valid": True, "checkpoints": 0, "unsigned": True}

        for cp in checkpoints:
            byte_offset = cp["byte_offset"]
            expected_hash = cp["hash"]

            # Verify content hash — covers all bytes before the checkpoint line
            actual_hash = hashlib.sha256(content[:byte_offset]).hexdigest()
            if actual_hash != expected_hash:
                return {
                    "valid": False,
                    "error": f"Content hash mismatch at turn {cp['turn']}",
                    "failed_at_turn": cp["turn"],
                    "byte_offset": byte_offset,
                }

            # Verify Ed25519 signature
            if not verify_signature(expected_hash, cp["sig"], cp["fp"]):
                return {
                    "valid": False,
                    "error": f"Signature verification failed at turn {cp['turn']}",
                    "failed_at_turn": cp["turn"],
                }

        # Check for unsigned trailing content (see §6)
        last_cp = checkpoints[-1]
        # Find the byte offset of the END of the last checkpoint line
        # (byte_offset is the START of the checkpoint line)
        last_cp_end = content.find(b"\n", last_cp["byte_offset"]) + 1
        if last_cp_end > 0 and last_cp_end < len(content):
            # There are bytes after the last checkpoint line — unsigned trailing content
            trailing_bytes = len(content) - last_cp_end
            return {
                "valid": False,
                "error": f"Unsigned content after last checkpoint "
                         f"({trailing_bytes} bytes after turn {last_cp['turn']})",
                "failed_at_turn": last_cp["turn"],
                "unsigned_bytes": trailing_bytes,
            }

        return {"valid": True, "checkpoints": len(checkpoints)}
```

### Integration with runner.py

The runner signs at two points:

```python
# In runner.py's main loop, ~line 108

signer = TranscriptSigner(thread_id, project_path / AI_DIR / "threads" / thread_id)

# Before the LLM call each turn (previous turn fully committed):
if cost["turns"] > 1:  # skip turn 1 — nothing to sign yet
    signer.checkpoint(cost["turns"] - 1)

# ... LLM call, tool dispatch, etc ...

# In _finalize():
signer.checkpoint(cost["turns"])  # sign the final turn
```

That's it. Two lines in the runner loop. The checkpoint is written to the same JSONL file via the same append mechanism. `transcript.md` renders `""` for checkpoint events (existing behavior for unknown event types). `reconstruct_messages()` skips them.

### Integration with resume/handoff

The verification happens at the trust boundary — before reconstructing messages:

```python
# In orchestrator.resume_thread(), before reconstruct_messages():
signer = TranscriptSigner(resolved_id, proj_path / AI_DIR / "threads" / resolved_id)
integrity = signer.verify()
if not integrity["valid"]:
    return {
        "success": False,
        "error": f"Transcript integrity check failed: {integrity['error']}. "
                 f"Cannot resume from tampered transcript.",
    }

# In orchestrator.handoff_thread(), before reading transcript.md:
signer = TranscriptSigner(thread_id, proj_path / AI_DIR / "threads" / thread_id)
integrity = signer.verify()
if not integrity["valid"]:
    return {
        "success": False,
        "error": f"Transcript integrity check failed: {integrity['error']}. "
                 f"Cannot hand off from tampered transcript.",
    }
```

---

## 5. What This Protects Against

| Threat                        | Without Signing                               | With Checkpoint Signing                                         |
| ----------------------------- | --------------------------------------------- | --------------------------------------------------------------- |
| Injected tool result          | Resume operates on fabricated data            | Verification fails, resume refused                              |
| Deleted safety refusal        | Resumed LLM doesn't know it refused before    | Content hash mismatch at deletion point                         |
| Modified LLM response         | Summary directive summarizes falsified output | Verification fails at tampered turn                             |
| Truncated transcript          | Resume starts from incomplete history         | Byte offset check fails                                         |
| Appended events after suspend | Extra events influence resumed context        | Last checkpoint's byte_offset < file size — detectable (see §6) |

### What this does NOT protect against

- **Compromise of the signing key** — if `~/.ai/keys/private.pem` is compromised, signatures can be forged. Same limitation as all Ed25519 signing in rye.
- **Events between checkpoints** — if the runner crashes mid-turn (after some tool results but before the next checkpoint), those events are unsigned. They'll be covered by the next checkpoint on resume. The gap is small — at most one turn's worth of events.
- **In-process tampering** — if the runner itself is compromised, it can write falsified events and sign them. Signing protects against offline/between-execution tampering, not runtime compromise.

---

## 6. Append Detection

A subtle case: events appended _after_ the last checkpoint but _before_ resume. The last checkpoint signed bytes `[0:N]`, but there are now bytes after the checkpoint line. The signed portion is still valid — but the trailing content is unsigned.

Because checkpoint events live in the JSONL stream, detection is precise: find the end of the last checkpoint line, check if there are bytes after it. This is integrated into `verify()` (see §4) — after all checkpoint hashes pass, it checks for trailing unsigned content.

For handoff (where `runner.py` is still running and signs the final turn before handing off), the last checkpoint is the final event — no trailing content. For resume (where the runner crashed mid-turn), there may be unsigned trailing events. The policy is configurable:

- **Strict** (default): refuse to resume from unsigned trailing events.
- **Lenient**: allow resume but log a warning. The trailing events are included in context but marked as unverified.

---

## 7. Performance

| Operation                             | Cost                                             | Frequency              |
| ------------------------------------- | ------------------------------------------------ | ---------------------- |
| SHA256 of transcript up to checkpoint | ~1ms for 100KB, ~10ms for 1MB                    | Once per turn          |
| Ed25519 sign                          | ~50μs                                            | Once per turn          |
| Write checkpoint event                | ~100μs (append + flush, same as any event)       | Once per turn          |
| Verify all checkpoints                | ~Nms (N = number of turns, dominated by hashing) | Once at resume/handoff |

For a typical 20-turn thread with ~50KB transcript, total signing overhead is ~20ms across the entire execution. Verification at resume is ~1ms. Negligible.

The O(n²) concern from "sign every event" is avoided. Each checkpoint hashes `[0:offset]`, but there are only ~N checkpoints (one per turn), not ~5N (one per event). The hashing work per checkpoint grows linearly with transcript size, and there are N checkpoints, so total work is O(N × avg_size) ≈ O(N²/2) — but N is turns (typically 5-50), not events. For a 50-turn thread, that's ~1275 hash operations of growing prefixes, still well under 100ms total.

---

## 8. Relationship to Graph State Signing

The graph state signing decision (STATE_GRAPH_AND_PROGRAMMATIC_EXECUTION.md §5) uses the existing `KnowledgeMetadataStrategy` — embed `<!-- rye:signed:... -->` in a markdown file that's fully rewritten each step. This works because graph state is a full-rewrite-per-step pattern (atomic write: temp → rename).

Transcripts are append-only, so they use the checkpoint-as-event pattern instead. Different embedding strategy for the same trust model:

| Aspect            | Graph State                                   | Transcript                                 |
| ----------------- | --------------------------------------------- | ------------------------------------------ |
| Write pattern     | Full rewrite per step                         | Append-only                                |
| Signature storage | Embedded header (`<!-- rye:signed:... -->`)   | Checkpoint event in JSONL stream           |
| Signing frequency | Every step                                    | Every turn (checkpoint)                    |
| Verification      | On resume                                     | On resume and handoff                      |
| Crypto primitives | `signing.py` (Ed25519, SHA256)                | Same                                       |
| Trust boundary    | Time gap between failure and resume           | Same                                       |
| New files         | None (uses existing knowledge infrastructure) | None (checkpoint events in existing JSONL) |

---

## 9. JSON Signing: `_signature` Field with Canonical Serialization

JSON has no comment syntax, so the embedded header pattern (`# rye:signed:...`) doesn't work. Instead, the signature lives as a `_signature` field in the JSON object itself.

### Mechanism

1. **Serialize** the JSON content _without_ `_signature`, using canonical formatting (sorted keys, no extra whitespace)
2. **Hash** the canonical bytes (SHA256)
3. **Sign** the hash (Ed25519)
4. **Store** the signature string as `_signature` in the same format as every other rye signature

```python
def sign_json(data: dict) -> dict:
    """Sign a JSON-serializable dict. Adds _signature field.

    Uses canonical serialization (sorted keys, compact separators)
    so the hash is reproducible on verification.
    """
    content = {k: v for k, v in data.items() if k != "_signature"}
    canonical = json.dumps(content, sort_keys=True, separators=(",", ":"))
    content_hash = hashlib.sha256(canonical.encode()).hexdigest()

    ed25519_sig = sign_hash(content_hash, private_pem)
    pubkey_fp = compute_key_fingerprint(public_pem)
    ts = generate_timestamp()

    data["_signature"] = f"rye:signed:{ts}:{content_hash}:{ed25519_sig}:{pubkey_fp}"
    return data


def verify_json(data: dict) -> bool:
    """Verify a signed JSON dict."""
    sig_str = data.get("_signature")
    if not sig_str:
        return False

    content = {k: v for k, v in data.items() if k != "_signature"}
    canonical = json.dumps(content, sort_keys=True, separators=(",", ":"))
    actual_hash = hashlib.sha256(canonical.encode()).hexdigest()

    # Parse rye:signed:TIMESTAMP:HASH:SIG:FP
    parts = sig_str.split(":")
    # parts = ["rye", "signed", TIMESTAMP, HASH, SIG, FP]
    expected_hash = parts[3]
    sig = parts[4]
    fp = parts[5]

    if actual_hash != expected_hash:
        return False
    return verify_signature(expected_hash, sig, fp)
```

### Example: Signed thread.json

```json
{
  "_signature": "rye:signed:2026-02-18T10:30:00Z:a3f2e8...:base64url...:440443d0...",
  "thread_id": "my-directive-1739820400",
  "directive": "workflows/code-review",
  "status": "running",
  "created_at": "2026-02-18T10:30:00Z",
  "updated_at": "2026-02-18T10:30:00Z",
  "model": "claude-sonnet-4-20250514",
  "capabilities": ["rye.execute.tool.*", "rye.load.knowledge.*"],
  "limits": { "turns": 20, "spend": 0.5, "depth": 3 }
}
```

### Why canonical serialization

The hash must be reproducible. If the same data produces different byte sequences on write vs verify, the hash won't match. `json.dumps(data, sort_keys=True, separators=(",", ":"))` is deterministic in Python — same data → same bytes → same hash. This is a well-established pattern (JWT, AWS Signature V4).

The `_signature` field is excluded before serialization, so its presence doesn't affect the hash. Writing the file and re-reading it produces the same canonical bytes as long as the non-signature fields don't change.

### Where this applies

| File          | Signed When                                     | Verified When                                                                        |
| ------------- | ----------------------------------------------- | ------------------------------------------------------------------------------------ |
| `thread.json` | Once at thread creation (`thread_directive.py`) | When child threads or graph walker read parent capabilities (§7 of state graph spec) |

`thread.json` is written once (with an update at completion for final status/cost). The security-critical fields (`capabilities`, `limits`) are set at creation and never change. Signing at creation is sufficient.

### Integration with thread_directive.py

```python
# In thread_directive.py, _write_thread_meta():
# After building the meta dict, before writing:
from persistence.transcript_signer import sign_json  # or a shared json_signing module
meta = sign_json(meta)

# Write as usual (atomic write: tmp → rename)
```

### Integration with graph walker (state_graph_walker.py)

```python
# In _resolve_execution_context(), after reading thread.json:
from persistence.transcript_signer import verify_json  # or shared module
meta = json.load(f)
if not verify_json(meta):
    return {
        "parent_thread_id": None,
        "capabilities": [],  # fail-closed — tampered parent context
        "limits": {},
        "depth": 0,
    }
```

---

## 10. Implementation Plan

### Phase 1: TranscriptSigner (~80 lines)

**New file:** `rye/agent/threads/persistence/transcript_signer.py`

- `checkpoint(turn)` — hash transcript up to current size, sign, append checkpoint event to JSONL
- `verify()` — extract checkpoint events from JSONL, verify all hashes + signatures + trailing content

### Phase 2: Runner Integration (~5 lines)

**Modified file:** `rye/agent/threads/runner.py`

- Import `TranscriptSigner`
- Call `signer.checkpoint(turn)` before each LLM call (turn > 1)
- Call `signer.checkpoint(final_turn)` in `_finalize()`

### Phase 3: Resume/Handoff Verification (~10 lines)

**Modified file:** `rye/agent/threads/orchestrator.py`

- Call `signer.verify()` before `reconstruct_messages()` in `resume_thread()`
- Call `signer.verify()` before reading `transcript.md` in `handoff_thread()`
- Return error with details if verification fails

### Phase 4: JSON Signing Utilities (~40 lines)

**New file or addition to:** `rye/agent/threads/persistence/transcript_signer.py` (or a shared `rye/utils/json_signing.py`)

- `sign_json(data)` — canonical serialize, hash, sign, add `_signature` field
- `verify_json(data)` — extract `_signature`, re-serialize, verify hash + Ed25519 sig

### Phase 5: thread.json Signing (~10 lines)

**Modified file:** `rye/agent/threads/thread_directive.py`

- Call `sign_json(meta)` in `_write_thread_meta()` before writing
- Verification is opt-in: graph walker verifies in `_resolve_execution_context()`, child threads can verify when reading parent capabilities

### Phase 6: Lenient Mode (~15 lines)

- Load policy from `coordination.yaml` (`transcript_integrity: strict | lenient`)
- Lenient mode: allow unsigned trailing events with warning
- Strict mode (default): refuse resume/handoff on any integrity failure

### What Does NOT Need Building

| Capability             | Already Exists                                                            |
| ---------------------- | ------------------------------------------------------------------------- |
| Ed25519 signing        | `lilux/primitives/signing.py`                                             |
| Key management         | `ensure_keypair()`, auto-trust                                            |
| SHA256 hashing         | `hashlib` (stdlib)                                                        |
| Trust store            | `TrustStore` for key verification                                         |
| Transcript writing     | `Transcript.write_event()` (unchanged)                                    |
| Message reconstruction | `Transcript.reconstruct_messages()` (unchanged — skips checkpoint events) |
| Resume/handoff flow    | `orchestrator.py` (add verification, don't restructure)                   |

### Total new code: ~160 lines

---

## 11. Universal Signing Coverage

With the checkpoint-as-event pattern for JSONL and the `_signature` field pattern for JSON, every file format in rye has a signing path:

| Format   | Signing Pattern                              | Files                                              |
| -------- | -------------------------------------------- | -------------------------------------------------- |
| Python   | `# rye:signed:...` header                    | Tools (`.py`)                                      |
| YAML     | `# rye:signed:...` header                    | Tools (`.yaml`), runtime configs                   |
| Markdown | `<!-- rye:signed:... -->` header             | Directives (`.md`), knowledge (`.md`), graph state |
| JSONL    | Checkpoint event (`event_type: checkpoint`)  | Transcripts (`transcript.jsonl`)                   |
| JSON     | `_signature` field (canonical serialization) | Thread metadata (`thread.json`)                    |

Same `rye:signed:TIMESTAMP:HASH:SIG:FP` format string everywhere. Same Ed25519 primitives. Same key management. Five embedding strategies for five file formats — each natural to its format, no sidecars, no format changes.
