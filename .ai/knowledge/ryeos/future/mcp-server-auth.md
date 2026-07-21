<!-- ryeos:signed:2026-07-21T00:24:56Z:8bced16b9d4477cbf0822209de20a1b1ceef0c32719c36b456fe45cda326383d:JulDRetCje752n3yOY3xtuDvuXSDayfexS+M9lD3ut/SSDqOw+1mr48gbvZarltQIqtVmDX+w+0RtAmmh2i6CA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: ryeos/future
name: mcp-server-auth
title: MCP Server Request Authentication
description: Deferred authentication boundary for any non-local MCP transport
entry_type: design
version: "1.0.0"
```

# MCP Server Request Authentication

## Status

Deferred. The current MCP adapter is local, single-user, stdio-only, and trusts
the operator's OS user.

## Current threat model

The MCP server wraps the installed `ryeos` CLI. It does not add an auth or
capability boundary of its own. Any caller that can reach the stdio transport can
invoke whatever the local CLI can invoke.

This is acceptable only for local IDE/agent integrations owned by the operator.

## Future direction

If the MCP server is ever exposed beyond local stdio, add a request-auth layer
before doing so:

1. signed requests from a delegated RyeOS principal;
2. audience binding to the target node/MCP adapter;
3. nonce/timestamp replay protection;
4. explicit capability projection from the delegated principal;
5. audit events that record caller, audience, command, and result status.

Do not expose the current MCP adapter over a network or as a multi-user service.
