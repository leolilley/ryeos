<!-- ryeos:signed:2026-05-31T08:15:56Z:7964f20dd51239204a828d6057bb546ea76a0d082296acd2adfd6234ccadbbb8:gRajYKV+f6Jcqf9czxFTf4TadXXXuRylzcnD71872ROdFhBzw15njy4rm+m8r8QBFHMa8P/rcnglpOjytT5CCw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/services
tags: [service, rebuild, projection]
version: "1.0.0"
description: Rebuild service reference.
---

# Service: rebuild

Invariant: rebuild reconstructs daemon projection state from CAS and signed registrations, and is an offline maintenance operation.

Use it after state corruption or migration when the append-only sources remain authoritative.
