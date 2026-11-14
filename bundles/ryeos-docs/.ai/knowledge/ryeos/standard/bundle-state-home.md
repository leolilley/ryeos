<!-- ryeos:signed:2026-11-14T06:02:00Z:c568e76fe5e06d261acc4ac09844337b17a633e6f010fd8106d057e9a1aae24d:ViP/pQpaNQLyuyMlUDi34yvz56d2+qTR9HMh7PyECK28kMTckP0hUH5mVx1jZNdy9HJ1hV2NddSg0Pom2IT0DA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard
tags: [bundle-state, runtime-authority, manifest, capabilities, isolation, backup]
version: "1.0.0"
description: >
  The node-owned bundle state home: durable, per-namespace directories a
  bundle's own verified code may keep state in, how the signed manifest declares
  it, which caller grant opens it, how isolation confines it, and how an
  operator backs it up, moves it and deletes it.
---

# Bundle state home

A bundle that needs durable state of its own (a central auth service keeping its
realms, an agent keeping its checkpoints) keeps it in a **bundle state home**:
a directory the node creates, owns and vouches for, under the app root:

```
<app_root>/.ai/state/bundle-state/<bundle_id>/<namespace>/   0700, daemon user
<app_root>/.ai/state/bundle-state/<bundle_id>/.home.json     node-signed record
```

The home is per **(bundle, namespace)**. A namespace is one segment,
`[a-z0-9][a-z0-9._-]{0,127}`. The bundle's code gets exactly one namespace per
launch; it never sees a sibling namespace or another bundle's home.

## Declaring it (signed data)

The bundle's signed manifest bounds which namespaces it may open:

```yaml
runtime_authority:
  state_home:
    namespaces:
      kind: fixed          # a closed list ...
      names: [default]
```

```yaml
runtime_authority:
  state_home:
    namespaces:
      kind: launch_param   # ... or any valid segment, chosen per launch
```

Wildcards are refused. `launch_param` exists for open-ended partitions such as
one namespace per realm.

An item asks for its namespace under `requires.capabilities.manifest`, like every
other runtime authority:

```yaml
requires:
  capabilities:
    manifest:
      runtime_authority:
        state_home:
          namespace: { from_param: realm }   # or a literal segment
          access: read_write                 # or read_only
```

The daemon mints the access capability (`ryeos.write.bundle-state.<bundle>/<ns>`
for `read_write`, `ryeos.read.bundle-state.<bundle>/<ns>` for `read_only`) only
when the installed bundle's signed manifest backs the namespace. Only an item
shipped by an installed bundle may open that bundle's home: a project item never
derives a bundle identity for this surface. An item cannot self-declare the
capability, because `bundle-state` is a reserved surface that composed
`declared` grants may not carry.

## Who may open a namespace

The bundle's code gets the mount. The **caller** must also hold
`ryeos.use.bundle-state.<bundle_id>/<namespace>` in its own grant. The daemon
reads this from the launch's caller scopes, never from the capabilities the
launch composed for the thread, so an item cannot lend itself access. Grant it
per namespace:

```bash
ryeos authorize-key --public-key <ed25519:...> --label app-backend \
  --scopes ryeos.use.bundle-state.central-auth/app-a
```

A caller granted `app-a` cannot open `app-b`. A follow child keeps only a
namespace its parent held.

## What the process sees

`RYEOSD_BUNDLE_STATE_HOME` (protocols `tool_callback` and `tool_streaming`) names
the directory, and is absent when the item declared no state home. Under
**enforced isolation** the exact namespace directory is mounted at
`/run/ryeos/bundle-state` (writable or read-only by the declared access), and
that is the value of the variable; the sibling namespaces and the app root are
not visible. Under **disabled isolation** there is no mount and the variable
names the real directory: the home is then a convention, not a boundary.
`ryeos node doctor` warns with `bundle_state_home_unenforced` while any home
exists on such a node.

## Refusals

Each refusal is typed and names its fix.

| Code | Meaning | Fix |
|---|---|---|
| `bundle_state_home_undeclared` | the installed bundle's signed manifest does not back the request | declare `runtime_authority.state_home`, sign the manifest, republish the bundle |
| `bundle_state_namespace_invalid` | the namespace (or its launch parameter) is not a valid segment | pass a valid segment |
| `bundle_state_namespace_denied` | the caller's grant lacks the `use` capability | `ryeos authorize-key ... --scopes ryeos.use.bundle-state.<bundle>/<namespace>` |
| `bundle_state_home_unavailable` | the directory cannot be created, verified or pinned (wider than 0700, a home record that does not verify) | check the state directory's permissions; run `ryeos node doctor` |

## Lifecycle

Removing a bundle **keeps** its home; `ryeos node doctor` lists such homes under
`bundle_state_orphaned` with their size. Deleting is only the offline operation
below. Garbage collection inside a namespace is the bundle's job; the node never
sweeps bundle data. `.ai/state` is node-owned, so homes are never captured by a
project snapshot or deployed by sync.

## Backup, move and delete (stopped node)

Three signed offline operations, local operator only, like
`ryeos node reset authorization`:

```bash
ryeos node bundle-state export --bundle <id> [--namespace <ns>] --output /abs/path.tar
ryeos node bundle-state import --archive /abs/path.tar [--replace-namespace <ns>]...
ryeos node bundle-state remove --bundle <id> [--namespace <ns>] --confirm
```

- `export` writes a tar of the namespace's files plus a node-signed manifest (file
  list with the sha256 and size of each file, bundle id, source node fingerprint,
  creation time). It carries regular files only and refuses links and special
  entries.
- `import` refuses unless the manifest verifies under this node's key or a
  pinned peer node key, the bundle is installed and its signed manifest declares a
  state home backing each namespace, and each target namespace is absent (or
  already identical). An existing, different namespace is replaced only when
  named with `--replace-namespace`. File modes are not carried: files are
  recreated private to the node's account.
- `remove` appends a ledger receipt naming the digest of the directory listing it
  will delete before deleting anything. Removing a namespace keeps the home's
  signed record; removing the whole home deletes it.

Moving a realm to a rebuilt host therefore needs no copy of the node's identity
or vault: export on the old node, pin the old node's key on the new one, install
the same bundle, import.

## Doctor rows

| Row | Status | Meaning |
|---|---|---|
| `bundle_state_homes` | ok / fail | inventory (bundle, namespaces, bytes, record verification); fail when a `.home.json` is missing, foreign or tampered, a namespace name is invalid, or a directory is wider than 0700 |
| `bundle_state_orphaned` | warn | homes whose bundle is not installed, with size |
| `bundle_state_home_unenforced` | warn | homes exist while isolation is not enforced |

See `ryeos/core/node/execution-isolation` for the mount, and
`ryeos/standard/bundle-events` for the other runtime authority families.
