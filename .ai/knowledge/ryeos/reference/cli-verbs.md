---
category: "ryeos/reference"
name: "cli-verbs"
description: "All CLI verbs, aliases, local commands, and how dispatch works"
---

# CLI Verbs Reference

The `ryeos` CLI dispatches through several layers.

## Layer 1: Local verbs (no daemon needed)

These run locally without contacting the daemon. They operate on operator state.

| Verb | Description | Key Flags |
|---|---|---|
| `ryeos init` | Bootstrap keys, install bundles | `--core-source`, `--standard-source`, `--system-space-dir`, `--trust-file`, `--force-node-key` |
| `ryeos publish <src>` | Sign bundle + rebuild CAS manifest | `--key`, `--owner`, `--registry-root`, `--no-trust-doc` |
| `ryeos trust pin --from <toml>` | Pin publisher key from trust doc | `--user-root` |
| `ryeos trust pin <fp> --pubkey-file <pem>` | Pin raw public key | `--owner`, `--user-root` |
| `ryeos vault put --name KEY` | Add secret to sealed store (reads stdin) | `--value-string` (insecure), `--system-space-dir` |
| `ryeos vault list` | List sealed secret keys (no values) | `--system-space-dir` |
| `ryeos vault remove KEY...` | Remove secrets | `--system-space-dir` |
| `ryeos vault rewrap` | Rotate vault keypair + re-seal | `--system-space-dir` |

## Layer 2: `ryeos execute <canonical-ref>`

Universal escape hatch. Sends the canonical ref to the daemon's `/execute` endpoint.

```bash
ryeos execute tool:ryeos/core/identity/public_key
ryeos execute directive:ryeos/core/init
```

Supports inline parameter binding from trailing args:
```bash
ryeos execute tool:ryeos/core/fetch key=directive:my/workflow
```

## Layer 3: Token dispatch (verb table)

Everything else is dispatched through the data-driven verb table. The CLI sends the tokens to the daemon, which resolves them via the alias/verb registry.

### Verb list (from core bundle)

| Verb | What it does |
|---|---|
| `execute` | Execute an item |
| `fetch` | Fetch an item by ref or query |
| `sign` | Sign items |
| `verify` | Verify item signatures |
| `status` | Daemon status |
| `identity-public-key` | Show node public key |
| `thread-list` | List threads |
| `thread-get` | Get thread details |
| `thread-chain` | Show thread state chain |
| `thread-tail` | Tail thread output |
| `thread-cancel` | Cancel running thread |
| `thread-children` | List child threads |
| `events-replay` | Replay events |
| `events-chain-replay` | Replay chain events |
| `commands-submit` | Submit a command |
| `bundle-install` | Install a bundle |
| `bundle-list` | List registered bundles |
| `bundle-remove` | Remove a bundle |
| `rebuild` | Rebuild item |
| `compose` | Compose items |
| `maintenance-gc` | Run garbage collection |
| `scheduler-list` | List scheduled jobs |
| `scheduler-show-fires` | Show scheduled fires |
| `scheduler-register` | Register a scheduled job |
| `scheduler-deregister` | Deregister a scheduled job |
| `scheduler-pause` | Pause scheduler |
| `scheduler-resume` | Resume scheduler |

### Aliases (shortcuts)

| Alias | Maps to |
|---|---|
| `s` | `status` |
| `f` | `fetch` |
| Various `*-` prefixed shortcuts | Expanded verb forms |

## Global flags

| Flag | Purpose |
|---|---|
| `-p, --project <PATH>` | Override project root (default: cwd) |
| `--debug` | Verbose tracing output |

## Environment variables

| Variable | Purpose |
|---|---|
| `RYEOS_STATE_DIR` | Override daemon state directory |
| `RYEOS_SYSTEM_SPACE_DIR` | Override system data directory |
| `RYEOS_CLI_KEY_PATH` | Path to CLI signing key |
| `RYEOSD_SOCKET_PATH` | Path to daemon UDS socket |
| `RYEOS_PUBLISHER_KEY` | Path to publisher signing key |
| `HOSTNAME` | Required for thread isolation |

## How dispatch works

```
CLI receives: ryeos fetch directive:my/workflow key=name value=test

1. Try local verbs: "fetch" is not local → skip
2. Try "execute" escape: "fetch" is not "execute" → skip
3. Token dispatch: send ["fetch", "directive:my/workflow", "key=name", "value=test"]
   to daemon's /execute endpoint
4. Daemon resolves "fetch" via alias registry → finds verb definition
5. Daemon binds parameters and executes
```
