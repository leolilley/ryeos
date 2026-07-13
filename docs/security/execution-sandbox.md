# Execution sandbox contract

RyeOS launches tool and runtime subprocesses through a node-owned Bubblewrap
policy on Linux. The policy is stored at `<app-root>/.ai/node/sandbox.yaml` and
is created by `ryeos init`. Items and bundles cannot weaken it.

## Enforcement boundary

Before every subprocess spawn, RyeOS loads and strictly parses the policy,
checks the configured backend, validates the complete constructed environment,
resolves writable roots, and wraps the existing `SubprocessSpec`. Execution is
refused when the policy is missing or invalid, its version is unsupported, the
backend is unavailable, an environment name is not allowed, or the working
directory is outside every writable root.

Bubblewrap starts a new session and namespaces, dies with its parent, mounts the
host filesystem read-only, creates private `/tmp`, `/dev`, and `/proc` mounts,
and bind-mounts only the configured writable roots as writable. Network access
is either shared with the host or isolated. Open-file and process limits are
applied when configured.

## Initial policy

The policy created by `ryeos init` is deliberately usable rather than a claim
of least privilege:

```yaml
version: 1
backend_path: /usr/bin/bwrap
allow_network: true
allow_host_read: true
writable_paths:
  - "{project}"
allowed_env:
  - "*"
max_open_files: 1024
max_processes: 256
```

`{project}` and `{cwd}` are the only path placeholders. Other writable paths
must be absolute. Environment entries are exact names, `*`, or prefix patterns
ending in `*`. Operators can disable networking and narrow environment names
without changing signed items.

## Guarantees and limits

The sandbox limits filesystem writes, namespace visibility, network access,
the subprocess environment, and selected resource counts. The initial policy
still permits reading host files, using the host network, and receiving the
daemon-constructed environment. Those are explicit operator defaults, not
permissions requested by an item.

This boundary does not make untrusted code harmless. It does not provide a
virtual machine, defend against kernel vulnerabilities, hide host files under
the initial policy, or impose CPU and memory quotas. RyeOS currently supports
this execution backend on Linux only and fails closed when Bubblewrap is not
installed. Container images and the Arch package include it as a runtime
dependency.

