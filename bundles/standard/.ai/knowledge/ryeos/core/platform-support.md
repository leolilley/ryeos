<!-- ryeos:signed:2026-07-14T01:54:46Z:650e5cb45baef13524ab899322372592ac949969715a2f7d05db2b1f9615a141:+yemIL8+GRJ9WwJNHtFv29u1MqNWr1NGegJ+vteAU5/J7TD+Ufdrg/Nv8gnglZ0CWyqKalUb9Oc/NO4QeLQICw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core
tags: [platform, linux, containers, compatibility, portability, support]
version: "1.0.0"
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
| Linux x86-64, glibc | Supported | Supported | Current bundle binaries target `x86_64-unknown-linux-gnu`. Bubblewrap is optional unless sandbox policy uses `mode: enforce`. |
| Linux x86-64 container (`linux/amd64`) | Supported | Supported | Default-disabled sandboxing needs no extra capability; enforced mode needs the documented namespace, seccomp, and AppArmor profile. Published images are single-platform. |
| Linux AArch64 | Not yet distributed | Not yet supported as a complete node distribution | Host-triple vocabulary exists, but official bundles do not ship AArch64 binaries. |
| Linux musl | Not yet distributed | Not yet supported as a complete node distribution | Official bundle binaries currently target glibc. |
| macOS and other Unix systems | Not distributed | Not supported | Some libraries have Unix fallbacks, but the complete distribution depends on Linux-target payloads and Linux-only activation/durability guarantees. Enforced Bubblewrap mode is Linux-only. |
| Windows | Not distributed | Not supported | Process isolation and several filesystem guarantees do not have a production backend. |

The supported Linux node requires atomic bundle replacement through
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
