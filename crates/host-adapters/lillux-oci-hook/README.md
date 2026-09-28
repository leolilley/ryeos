# Contained Docker runtime adapter

This package provides the administrator-installed OCI lifecycle hook and an
explicit Docker runtime entry. The adapter is under installed qualification;
source tests do not establish that a host supplies the required containment.

The ordinary `ryeos-hosted-workflow` image continues to use Docker's ordinary
runtime. Only `ryeos-contained-workflow` selects this adapter. The adapter does
not make restricted platforms capable of delegated process control. Externally
fenced and explicitly trusted workers retain their separate admission contracts.

## Host registration

The checked-in installer registers the adapter without changing the default
runtime or restarting Docker:

```sh
sudo python3 scripts/pkg/install-contained-docker-runtime.py \
  --binary /absolute/path/to/ryeos-lillux-oci-hook \
  --sha256 <verified-binary-sha256> \
  --runc /usr/bin/runc --confirm
```

It installs immutable bytes beneath `/usr/lib/ryeos/contained-oci/<sha256>/`,
initializes the hook journal directory, validates the merged configuration with
`dockerd --validate`, and retains a content-addressed backup of any previous
configuration. A different existing registration is refused: replacing the hook
while containers retain lifecycle obligations requires an explicit migration.
For a daemon using a non-default configuration path, pass its exact `--config`.
Activate the configuration through the host's service manager only after checking
the running daemon's flags and other containers. This installer does not create
nodes or assert installed containment.

For manual packaging, the equivalent registration contract follows.

Install the executable at a root-owned, non-writable path, for example
`/usr/lib/ryeos/ryeos-lillux-oci-hook`, with root-owned protected parent directories.
Select an absolute administrator-installed `runc` executable. Initialize hook
state once with `ryeos-lillux-oci-hook install-host-state` as the administrator.

Merge this **additional runtime** into the Docker daemon's existing configuration:

```json
{
  "runtimes": {
    "ryeos-contained": {
      "path": "/usr/lib/ryeos/ryeos-lillux-oci-hook",
      "runtimeArgs": ["docker-runtime", "--runc", "/usr/bin/runc", "--"]
    }
  }
}
```

Keep Docker's default runtime unchanged. Preserve existing configuration and
coordinate daemon configuration activation with other containers on the host.
Docker documents this interface in its
[alternative runtime guide](https://docs.docker.com/engine/daemon/alternative-runtimes/).

Select `--runtime ryeos-contained` for the contained image, or
`runtime: ryeos-contained` on its Compose service. Use an immutable image digest.
Registration and node preparation are separate operations. After activating the
runtime, use the supported node setup command with an already loaded immutable
contained image:

```sh
sudo python3 scripts/pkg/setup-contained-docker-node.py \
  --name ryeos-contained-development \
  --image <repository@sha256:digest-or-local-sha256:image-id> \
  --port 7447 --confirm
```

The command creates private app/project directories beneath
`/var/lib/ryeos/contained-nodes/<name>`, records their identities, runs offline
initialization as UID/GID 10001 from the pinned image and signed contained profile,
then starts the fixed bootstrap through the opt-in runtime. It publishes only a
loopback endpoint and waits up to 60 seconds for daemon admission readiness.
Initialization and daemon configuration use the same stored `[::]:8000` bind.
The project directory is mounted at `/projects` for normal remote project binding.

Nested RyeOS sandboxing requires the outer Docker seccomp/AppArmor filters to
allow namespace setup. This dedicated deployment sets those filters unconfined;
it does not select privileged mode or host PID/mount namespaces. The signed RyeOS
isolation policy and installed process-control probes remain mandatory. A host
that forbids the required namespaces refuses during initialization/readiness.

Repeated setup checks the retained directory identities and exact container
configuration, and reuses an existing matching node. It never resets policies,
changes owners on an existing app root, deletes state, recreates a mismatched
container, or automatically migrates images/ports. Interrupted setup before its
record is complete refuses further provisioning. Restart uses the runtime hooks
and their existing cleanup/reuse checks. Installed lifecycle qualification and
the authenticated worker proof still need to run on the selected host.

## Hook artifact identity

Release builds embed `.ryeos_contained_oci_hook_build` with exact version, source,
timestamp, target and profile. `scripts/release/contained-oci-hook-artifact.py`
packages or verifies those bytes without executing the adapter. Archives contain
exactly the hook, LICENSE and RYEOS-BUILD. Verification checks the checksum's exact
filename, bounded decompression, regular-file inventory, modes, owners, ELF
architecture and embedded identity. Development builds cannot pass exact release
verification. Checksums establish integrity; release authentication remains the
publication channel's responsibility. Official promotion remains disabled until
installed qualification is complete.

## Lifecycle

On `create`, the adapter checks the fixed root bootstrap and private PID/mount
namespaces, attaches bounded `prestart` and `poststop` hooks to the protected OCI
bundle, then execs the selected runtime with its original arguments. User-namespace
remapping is refused because the installed account contract fixes host UID/GID
10001. Existing hooks are preserved. Repeated attachment is idempotent only when
the installed hook already matches exactly.

The hook observes the paused init process, prepares its child delegation, and
publishes the protected binding. The image consumes that binding and drops to the
controller account. Poststop retains the existing host-observed death and volume
lease checks. Interrupted cleanup requires `recover <container-id>`; registration
does not clear or override leases. Checkpoint/restore and direct `run` are refused
by this Docker adapter; Docker uses `create` followed by `start`.

The host-side runtime is administrator authority. Access to Docker's control
socket remains host authority; do not expose it to the worker. The adapter checks
bootstrap shape, while the deployment administrator pins and authenticates the
image. An entrypoint match is not image provenance or installed qualification.
