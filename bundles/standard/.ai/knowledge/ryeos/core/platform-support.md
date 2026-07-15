<!-- ryeos:signed:2026-07-15T08:13:20Z:b4c133fe585295912eb4f898cd9d45ad23d162464f18b68e8af1fe4d35076da6:SaOxGnHa2M5wBS91JYGC6dQM1bDVNDCRRTbS1TuTycL7LDjW8CyE9vsz/8ERTwgfS7ZI5Bmdc8BlaBzSGRfSCQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core
tags: [platform, linux, containers, compatibility, portability, support]
version: "1.1.0"
description: >
  Supported RyeOS host, architecture, packaging, container, sandbox, and
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
| Linux 6.9+, x86-64, glibc | Supported | Supported | Current bundle binaries target `x86_64-unknown-linux-gnu`. Bubblewrap is optional unless sandbox policy uses `mode: enforce`. |
| Linux 6.9+ x86-64 container (`linux/amd64`) | Supported | Supported | The host kernel supplies the node's pidfd contract. Default-disabled sandboxing needs no extra capability; enforced mode needs the documented namespace, seccomp, and AppArmor profile. Published images are single-platform. |
| Linux AArch64 | Not yet distributed | Not yet supported as a complete node distribution | Host-triple vocabulary exists, but official bundles do not ship AArch64 binaries. |
| Linux musl | Not yet distributed | Not yet supported as a complete node distribution | Official bundle binaries currently target glibc. |
| macOS and other Unix systems | Not distributed | Not supported | Some libraries have Unix fallbacks, but the complete distribution depends on Linux-target payloads and Linux-only activation/durability guarantees. Enforced Bubblewrap mode is Linux-only. |
| Windows | Not distributed | Not supported | Process isolation and several filesystem guarantees do not have a production backend. |

The supported Linux node requires kernel 6.9 or newer, including
`PIDFD_SIGNAL_PROCESS_GROUP` and `SO_PEERPIDFD`. Daemon startup probes both and
fails before launching work when either is unavailable; there is no reusable
numeric-PID signal fallback. It also requires atomic bundle replacement through
`renameat2(RENAME_EXCHANGE)`. When sandbox policy is `mode: enforce`,
an unprivileged Bubblewrap 0.11.0-or-newer installation must also be present
and permitted to create its configured user and other namespaces. Setuid, setgid, and
file-capability Bubblewrap executables are refused because RyeOS executes an
exact private capture. See [Execution Sandbox](node/execution-sandbox.md).

## Compatibility boundaries

- Native bundle executables require matching payloads under
  `.ai/bin/<target-triple>/`.
- Portable descriptors, CAS, signatures, and protocols do not emulate a
  missing native runtime.
- A missing sandbox backend is permitted only while node policy is disabled;
  enforce mode fails closed.
- Release workflows currently publish `linux/amd64` images and
  `x86_64-unknown-linux-gnu` bundle payloads.
- Compiling a source checkout on another platform does not make that platform
  a supported node. Security and durability primitives fail closed where they
  guard execution or bundle activation.

Production support is declared only when the full path exists: signed bundle
artifacts, durable installation, release packaging, CI coverage, and a tested
enforced-sandbox profile where that capability is claimed.

Future non-Linux isolation must not imitate Bubblewrap flags or map unlike host
primitives to one broad `sandboxed` claim. RyeOS should match typed isolation
requirements against node-owned backend capabilities and report exactly which
boundary was realized. The deferred design lives in
`ryeos/future/data-driven-execution-isolation-backends`. Until a platform has a
packaged adapter, backend integration tests, and the rest of the node's
durability requirements, it remains unsupported rather than silently falling
back to direct execution.
