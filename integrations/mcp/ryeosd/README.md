# ryeosd-mcp

MCP server adapter for Rye OS. Exposes `ryeos` CLI verbs as an MCP `cli` tool.

**Threat model**: local single-user only. See the module-level
docstring at the top of `ryeosd_mcp/server.py` and the deferred
per-request auth design at `docs/future/mcp-server-auth.md`.
