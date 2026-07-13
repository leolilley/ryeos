# Execution sandbox contract

RyeOS can launch tool and runtime subprocesses through a node-owned Bubblewrap
policy on Linux. Sandboxing is opt-in through the flat
`sandbox_enabled: true` setting in `<app-root>/.ai/node/config.yaml`; it is
disabled when the setting is absent or false. The policy is stored at
`<app-root>/.ai/node/sandbox.yaml` and is created by `ryeos init`. Items and
bundles cannot enable or weaken it.

## Enforcement boundary

When enabled, before every subprocess spawn RyeOS loads and strictly parses the policy,
checks the configured backend, validates the complete constructed environment,
resolves writable roots, and wraps the existing `SubprocessSpec`. Execution is
refused when the policy is missing or invalid, its version is unsupported, the
backend is unavailable, an environment name is not allowed, or the working
directory is outside every writable root.

Bubblewrap starts a new session and namespaces, dies with its parent, mounts the
host filesystem read-only, creates private `/tmp`, `/dev`, and `/proc` mounts,
and bind-mounts only the configured writable roots as writable. Network access
is either shared with the host or isolated. A configured `max_open_files` limit
is applied at the Lillux spawn boundary with `RLIMIT_NOFILE` and inherited by
Bubblewrap and the sandboxed process.

The version 1 policy still accepts `max_processes` so existing operator policy
files continue to load, but RyeOS does not enforce it as a per-sandbox process
limit. Applying `RLIMIT_NPROC` would limit the daemon's real UID rather than one
sandbox and can be bypassed by privileged processes, so `ryeos node doctor`
reports a configured value as a warning. Per-sandbox process enforcement is
deferred to delegated cgroup v2 `pids.max` support.

## Activation and initial policy

Enable the boundary in the node's existing flat configuration:

```yaml
sandbox_enabled: true
```

The default is `false`. The policy created by `ryeos init` is inert until the
operator enables it.

The policy created by `ryeos init` is deliberately usable rather than a claim
of least privilege:

```yaml
version: 1
backend_path: /usr/bin/bwrap
allow_network: true
writable_paths:
  - "{project}"
allowed_env:
  - "*"
max_open_files: 1024
```

`{project}` and `{cwd}` are the only path placeholders. Other writable paths
must be absolute. Environment entries are exact names, `*`, or prefix patterns
ending in `*`. Operators can disable networking and narrow environment names
without changing signed items.

## Guarantees and limits

The sandbox limits filesystem writes, namespace visibility, network access,
the subprocess environment, and selected resource counts. The initial policy
still permits reading the active project and its exact executable, using the
host network, and receiving the daemon-constructed environment. The app root,
vault and signing keys, and operator home are not mounted. These are explicit
operator defaults, not permissions requested by an item.

`ryeos node doctor` reports the configured network posture, environment policy,
writable paths, and the runtime mechanism for each resource limit. It also
validates `max_open_files` against the doctor process's current hard limit and
warns if that process rejects it; a daemon service or container can have a
different hard limit, so the report identifies this as a context-specific
check. A configured `max_processes` also produces `WARN`, not `FAIL`, because
the field remains valid policy syntax but has no per-sandbox enforcement
mechanism yet.

This boundary does not make untrusted code harmless. It does not provide a
virtual machine, defend against kernel vulnerabilities, impose CPU or memory
quotas, or currently impose a per-sandbox process-count quota. RyeOS currently
supports this execution backend on Linux only and fails closed when Bubblewrap
is not installed. Container images and the Arch package include it as a runtime
dependency.
