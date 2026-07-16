<!-- ryeos:signed:2026-07-16T03:44:59Z:f2136ecfb3022a0f469ca4943cd2ba29c4534bd5eedfdfebf1e57505a6616232:PjqllpWscU2EkocidGkiYRu6wqqPuz6wEzkVqE5/fedvb7isxgcsAXt8e+0dY3CE3YIGJfBOcObqiz0fnr3UBg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core
tags: [platform, linux, containers, compatibility, portability, support]
version: "1.2.0"
description: >
  Supported RyeOS host, architecture, packaging, container, isolation, and
  filesystem boundaries.
---

# Platform Support

RyeOS uses *portable verified execution* to describe signed execution data:
items, bundles, threads, CAS objects, and authorization can move between
compatible nodes without borrowing ambient trust from the destination. It does
not mean every distribution currently runs on every operating system or CPU.

## Supported production runtime

| Platform | Package/image support | Node execution support | Notes |
|---|---|---|---|
| Linux 6.9+, x86-64, glibc | Supported | Supported | Current bundle binaries target `x86_64-unknown-linux-gnu`. The selected signed isolation bundle supplies its adapter and launcher artifacts. |
| Linux 6.9+ x86-64 container (`linux/amd64`) | Supported | Supported | The host kernel supplies the node's pidfd contract. Disabled isolation needs no extra capability; enforced mode needs the documented namespace, seccomp, and AppArmor profile. Published images are single-platform. |
| Linux AArch64 | Not yet distributed | Not yet supported as a complete node distribution | Host-triple vocabulary exists, but official bundles do not ship AArch64 binaries. |
| Linux musl | Not yet distributed | Not yet supported as a complete node distribution | Official bundle binaries currently target glibc. |
| macOS and other Unix systems | Not distributed | Not supported | Some libraries have Unix fallbacks, but the complete distribution depends on Linux-target payloads and Linux-only activation/durability guarantees. The current enforced isolation bundle is Linux-only. |
| Windows | Not distributed | Not supported | Process isolation and several filesystem guarantees do not have a production backend. |

The supported Linux node requires kernel 6.9 or newer, including
`PIDFD_SIGNAL_PROCESS_GROUP` and `SO_PEERPIDFD`. Daemon startup probes both and
fails before launching work when either is unavailable; there is no reusable
numeric-PID signal fallback. It also requires atomic bundle replacement through
`renameat2(RENAME_EXCHANGE)`. When isolation policy is `mode: enforce`, the
selected registered bundle must provide a trusted adapter and launcher for the
host triple. The shipped Linux bundle carries Bubblewrap 0.11.2 with libcap
linked into the payload; it does not depend on a host Bubblewrap or libcap
installation. The host must permit its configured user and other namespaces.
Setuid, setgid, and file-capability executables are refused before RyeOS copies
the verified bytes into an immutable private capture. See [Execution
Isolation](node/execution-isolation.md).

## Compatibility boundaries

- Native bundle executables require matching payloads under
  `.ai/bin/<target-triple>/`.
- Portable descriptors, CAS, signatures, and protocols do not emulate a
  missing native runtime.
- A missing selected isolation backend is permitted only while node policy is disabled;
  enforce mode fails closed.
- Release workflows currently publish `linux/amd64` images and
  `x86_64-unknown-linux-gnu` bundle payloads.
- Compiling a source checkout on another platform does not make that platform
  a supported node. Security and durability primitives fail closed where they
  guard execution or bundle activation.

Production support is declared only when the full path exists: signed bundle
artifacts, durable installation, release packaging, CI coverage, and a tested
enforced-isolation profile where that capability is claimed.

Non-Linux isolation must not imitate Bubblewrap flags or map unlike host
primitives to one broad claim. RyeOS matches typed isolation requirements
against node-owned backend capabilities and reports exactly which boundary was
realized. Until a platform has a
packaged adapter, backend integration tests, and the rest of the node's
durability requirements, it remains unsupported rather than silently falling
back to direct execution.
