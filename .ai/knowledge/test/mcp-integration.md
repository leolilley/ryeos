<!-- ryeos:signed:2026-05-25T04:23:46Z:d6c6498edcda9dfa72a9aa85a04c73096d9eb5ae81ff7a7a9738a9a2895afaef:eTOOnms75fdRvgRGqgS5YL1DiUBLuvW7Ks/0CrYuLeKGeu9HKuidrEBvcuckU2YeQalHCdCCGktaddpFz1aUAw==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->
---
kind: knowledge
id: test/mcp-integration
version: "1.0.0"
tags: [mcp, testing, v2]
---

# MCP Integration Notes

The ryeos-rust-v2 MCP server is a thin Python wrapper that exposes the `ryeos` CLI as a single `cli` tool.

## Architecture

- MCP server: `ryeos-next/integrations/mcp/ryeosd/`
- CLI binary: `ryeos-next/target/release/ryeos`
- Transport: stdio (local single-user)
- Tool name: `cli`
- Input: `args[]`, `project_path`, `timeout_s`
- Output: `exit_code`, `stdout`, `stderr`, `json` (parsed when possible)
