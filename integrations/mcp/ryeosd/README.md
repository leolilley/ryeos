# ryeosd-mcp

MCP server adapter for Rye OS. Exposes `ryeos` CLI verbs as an MCP `cli` tool.

**Threat model**: local single-user only. See the module-level
docstring at the top of `ryeosd_mcp/server.py` and the deferred
per-request auth design at
`.ai/knowledge/ryeos/future/mcp-server-auth.md` in the repository root.

**Verbs that need a human.** Every `ryeos` the server starts carries
`RYEOS_CALLER=agent`, and the CLI then refuses trust-changing verbs (endorse,
sign, key and grant changes, secrets, bundle install, remote trust, approvals,
credentials, and any verb that reaches the same service, including
`execute <ref>`) with `verb_needs_human_terminal`. This guards against an agent
steered by injected text; it does not contain a hostile process on a node that
does not enclose its runs. See "Verbs that need a human's terminal" in the CLI
reference.
