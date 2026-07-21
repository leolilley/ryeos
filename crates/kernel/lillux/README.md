# Lillux

> Microkernel for [RYE OS](https://github.com/leolilley/ryeos) — Execute, Memory, Identity, Time.

Lillux is a native Rust binary that provides the OS-level primitives beneath RYE. Every tool call in RYE eventually bottoms out in a Lillux primitive. It handles process lifecycle, content-addressed storage, cryptographic identity (signing, key management, and sealed secret envelopes), and time — nothing more. RYE never sees your secrets or environment variables; that happens at the Lillux level, below RYE entirely.

Filesystem atomicity, flush, locking, and platform claims are defined in the
[repository durability matrix](../../../.ai/knowledge/ryeos/development/filesystem-durability.md).
Batch writes and recursive removal are not multi-file atomic transactions, and
directory durability is currently stronger on Unix than on non-Unix systems.

## Primitives

| Primitive    | Command                                          | What it does                                                                     |
| ------------ | ------------------------------------------------ | -------------------------------------------------------------------------------- |
| **Execute**  | `lillux exec`                                    | Run/spawn/kill/status — full process lifecycle with timeouts                     |
| **Memory**   | `lillux cas`                                     | Content-addressed storage — store, fetch, verify, existence check                |
| **Identity** | `lillux identity`                                | Ed25519 signing/verification, keypair generation, sealed secret envelopes        |
| **Time**     | `lillux time`                                    | Wall-clock timestamp, sleep                                                      |

## Attachment-before-execution API

RyeOS uses Lillux's Rust process API to make durable process ownership precede
target execution:

```rust
let pending = lillux::spawn_awaiting_attachment(request)?;
persist_exact_identity(pending.pid(), pending.pgid(), pending.pidfd())?;
let running = pending.release_after_attachment()?;
let result = running.wait()?;
```

`ProcessAwaitingAttachment` is a distinct type state. It can be released after
the caller durably attaches its exact target identity, or aborted and reaped;
it cannot be waited as a running process. Release consumes it and returns a
`RunningProcess`.

Direct execution uses a native pre-exec hold. A supervised isolation request
must carry a backend target hold and exact target-status channel or the
attachment spawn is refused. Cleanup retains the pidfd and unreaped process
group leader until exact target exit, group quiescence, and leader reap are
proven. This lifecycle primitive does not require or select an isolation
backend.

See the RyeOS knowledge entry
`knowledge:ryeos/core/execution/attachment-before-execution` for the durable
daemon contract layered on this primitive.

## Install

```
pip install lillux
```

This installs the compiled Rust binary via [maturin](https://github.com/PyO3/maturin). Requires Python ≥ 3.10.

**From source:**

```bash
cd crates/kernel/lillux
cargo build --release
```

## Usage

All commands return JSON to stdout:

```bash
# Process execution
lillux exec run --cmd python --arg -c --arg "print('hello')"
lillux exec spawn --cmd sleep --arg 60
lillux exec status --pid 12345
lillux exec kill --pid 12345

# Content-addressed storage
echo '{"key": "value"}' | lillux cas store --root /tmp/cas
lillux cas fetch --root /tmp/cas --hash <sha256>
lillux cas verify --root /tmp/cas --hash <sha256>
echo "raw bytes" | lillux cas store --root /tmp/cas --blob

# Identity — signing
lillux identity sign --key-dir ~/.local/share/ryeos/.ai/config/keys/signing --hash <sha256>
lillux identity verify --hash <sha256> --signature <base64url> --public-key public_key.pem

# Identity — keypairs
lillux identity keypair generate --key-dir ~/.local/share/ryeos/.ai/config/keys/signing
lillux identity keypair fingerprint --public-key public_key.pem
lillux identity keypair box-fingerprint --public-key box_pub.pem

# Identity — sealed envelopes
echo '{"API_KEY":"sk-..."}' | lillux identity envelope seal --box-pub /path/to/box_pub.pem
cat envelope.json | lillux identity envelope open --key-dir ~/.local/share/ryeos/.ai/config/keys/signing
echo '{"API_KEY":"sk-..."}' | lillux identity envelope validate
cat envelope.json | lillux identity envelope inspect

# Time
lillux time now
lillux time after --ms 1000
```

## Architecture

Lillux is intentionally minimal — a single static binary with no runtime dependencies. Objects are stored with sharded paths (`root/objects/ab/cd/<hash>.json`), blobs separately (`root/blobs/ab/cd/<hash>`). JSON objects use the RyeOS canonical encoding before SHA-256 hashing: compact JSON, decoded object keys in lexicographic order, lowercase `\u` escapes for every non-ASCII scalar, and the exact `serde_json::Number` rendering. These bytes are an immutable persistence protocol, not RFC 8785/JCS; another implementation must reproduce them exactly rather than substituting its platform's default serializer.

The Identity primitive includes sealed secret envelopes using single-use X25519 key agreement with HKDF-SHA256 key derivation and ChaCha20Poly1305 AEAD, with safety limits on env variable count, value size, and total payload. Reserved environment names and prefixes are rejected to prevent injection.

Cross-platform: Unix (setsid for daemon spawning, SIGTERM/SIGKILL) and Windows (CREATE_NEW_PROCESS_GROUP, TerminateProcess).

## License

MIT
