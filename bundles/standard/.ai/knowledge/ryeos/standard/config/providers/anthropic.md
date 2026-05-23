<!-- ryeos:signed:2026-05-23T12:11:51Z:2015ec4eab57b1cd3159eb037a5ee0a966aadfe413c76305608505ba94c5496c:8ba6U0FUXAGL8aZborFkffAnFLYrk8cSfIeYu5i31KtxdpGzD/VxX66268MgHnbSIVjM61iHxpgF8zhHnGQTDQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard/config/providers
tags: [provider, anthropic, models]
version: "1.0.0"
description: Anthropic provider config reference.
---

# Provider Config: anthropic

Invariant: the Anthropic provider config describes the direct Anthropic Messages API schema, auth header, streaming mode, model profiles, and pricing defaults.

Use it when a directive explicitly selects Anthropic or a routing tier points to it. Provider configs are security-sensitive because they control outbound URL and auth env vars; project-root provider contributions are rejected unless explicitly trusted.
