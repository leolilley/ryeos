# Platform support

RyeOS uses *portable verified execution* to describe its signed execution data:
items, bundles, threads, CAS objects, and authorization can move between
compatible nodes without borrowing ambient trust from the destination. It does
not mean every RyeOS distribution currently runs on every operating system or
CPU architecture.

## Supported production runtime

| Platform | Package/image support | Node execution support | Notes |
|---|---|---|---|
| Linux x86-64, glibc | Supported | Supported | Current bundle binaries use the `x86_64-unknown-linux-gnu` target. Bubblewrap is mandatory for subprocess execution. |
| Linux x86-64 container (`linux/amd64`) | Supported | Supported with the documented namespace permissions | Published daemon images are currently single-platform. |
| Linux AArch64 | Not yet distributed | Not yet supported as a complete node distribution | Host-triple vocabulary exists, but official bundles do not yet ship AArch64 binaries. |
| Linux musl | Not yet distributed | Not yet supported as a complete node distribution | Official bundle binaries currently target glibc. |
| macOS and other Unix systems | Not distributed | Not supported | Some libraries have explicit Unix fallbacks, but the node requires the Linux Bubblewrap sandbox and Linux-only bundle activation guarantees. |
| Windows | Not distributed | Not supported | Process isolation and several filesystem guarantees do not have a production backend. |

The supported Linux node still depends on kernel and filesystem capabilities.
Bubblewrap must be able to create the configured namespaces, and atomic bundle
replacement requires `renameat2(RENAME_EXCHANGE)` support on the filesystem.
The [sandbox contract](security/execution-sandbox.md) and
[filesystem durability matrix](architecture/filesystem-durability.md) define
those requirements precisely.

## Compatibility boundaries

- A bundle containing native executables is compatible only with a node that
  has matching binaries under `.ai/bin/<target-triple>/`.
- Descriptor, CAS, signature, and protocol portability does not emulate a
  missing native runtime or sandbox backend.
- The current release workflows publish `linux/amd64` images and
  `x86_64-unknown-linux-gnu` bundle payloads. Adding another architecture
  requires publishing and signing the complete bundle binary set for it.
- A source checkout compiling on another platform does not make that platform
  a supported node. Unsupported security or durability primitives fail closed
  where they guard execution or bundle activation.

## Development versus production

Cross-platform library work is welcome, and lower-level crates document their
fallback behavior. Production support is declared only after the full path is
available: signed bundle artifacts, sandboxed execution, durable installation,
release packaging, and CI coverage. Until then, do not weaken verification or
silently bypass a missing platform primitive to make a node appear runnable.
