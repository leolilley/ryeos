<!-- ryeos:signed:2026-11-16T23:09:12Z:d2f34d74686ea3c3e97bd5e20e6202eb5d4f94a3e8650575f39d75371b4a0988:oDzb2JRc0rjqMU2jXEUZ0tAZQwMxkLUOKmcU4dm4IJLVGHUvmBY4Y+JSlnRvH7vm1Iclb9sKBhL5o8c5Gt4BAQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard/config/providers
tags: [provider, anthropic, models]
version: "1.0.0"
description: Anthropic provider config reference.
---

# Provider Config: anthropic

Invariant: the Anthropic provider config describes the direct Anthropic Messages API schema, auth header, streaming mode, model profiles, and pricing defaults.

Use it when a directive explicitly selects Anthropic or a routing tier points to it. Provider configs are security-sensitive because they control outbound URL and auth env vars; project-root provider contributions are rejected. To point Anthropic at another endpoint, override `transport.base_url` (the full `/v1/messages` URL) and `extra.setup.validation.url` in an operator-signed node-space file; see "Changing a provider's endpoint" in `ryeos/standard/model-providers`.
