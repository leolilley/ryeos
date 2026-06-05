# MCP server request authentication

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
