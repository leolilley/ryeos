# Rye OS Documentation

Welcome to the Rye OS documentation. Use the sections below to navigate.

---

## Setup

- [Installation](setup/installation.md)
- [MCP Client Configuration](setup/mcp-client-configuration.md)
- [User Space and Project Space](setup/user-space-and-project-space.md)

## Guides

- [Quickstart](guides/quickstart.md)
- [Working with Items](guides/working-with-items.md)
- [Authoring Directives](guides/authoring-directives.md)
- [Authoring Tools](guides/authoring-tools.md)
- [Authoring Knowledge](guides/authoring-knowledge.md)
- [Using the Registry](guides/using-the-registry.md)
- [Agent Threads](guides/agent-threads.md)

## Concepts

- [Architecture Overview](concepts/architecture-overview.md)
- [Item Model](concepts/item-model.md)
- [Toolchains and Execution](concepts/toolchains-and-execution.md)
- [Lockfiles](concepts/lockfiles.md)
- [Spaces and Precedence](concepts/spaces-and-precedence.md)

## Security

- [Security Overview](security/security-overview.md)
- [Content Signing](security/content-signing.md)
- [Keys and Trust](security/keys-and-trust.md)
- [TOFU Registry Pinning](security/tofu-registry-pinning.md)
- [Capability Tokens](security/capability-tokens.md)
- [Agent Thread Safety](security/agent-thread-safety.md)
- [Injection Hardening](security/injection-hardening.md)

## Platform: RYE

- [Overview](platform/rye/overview.md)
- [MCP Server](platform/rye/mcp-server.md)

### MCP Tools

- [Overview](platform/rye/mcp-tools/overview.md)
- [Execute](platform/rye/mcp-tools/execute.md)
- [Load](platform/rye/mcp-tools/load.md)
- [Search](platform/rye/mcp-tools/search.md)
- [Sign](platform/rye/mcp-tools/sign.md)

### Handlers

- [Overview](platform/rye/handlers/overview.md)

### Executor

- [Overview](platform/rye/executor/overview.md)
- [Chain Validation](platform/rye/executor/chain-validation.md)
- [Resolution](platform/rye/executor/resolution.md)

### Utilities

- [Metadata Manager](platform/rye/utilities/metadata-manager.md)
- [Trust Store](platform/rye/utilities/trust-store.md)
- [Validators and Parsers](platform/rye/utilities/validators-and-parsers.md)

## Platform: Lilux

- [Overview](platform/lilux/overview.md)

### Primitives

- [Overview](platform/lilux/primitives/overview.md)
- [Subprocess](platform/lilux/primitives/subprocess.md)
- [HTTP Client](platform/lilux/primitives/http-client.md)
- [Signing](platform/lilux/primitives/signing.md)
- [Integrity](platform/lilux/primitives/integrity.md)
- [Lockfile](platform/lilux/primitives/lockfile.md)

### Runtime Services

- [Overview](platform/lilux/runtime-services/overview.md)
- [Auth](platform/lilux/runtime-services/auth.md)
- [Env Resolver](platform/lilux/runtime-services/env-resolver.md)

### Schemas

- [Overview](platform/lilux/schemas/overview.md)

## Bundled Content

- [Overview](bundled-content/overview.md)
- [Layout](bundled-content/layout.md)
- [Directives](bundled-content/directives.md)
- [Knowledge](bundled-content/knowledge.md)

### Tools

- [Overview](bundled-content/tools/overview.md)
- [Categories](bundled-content/tools/categories.md)
- [Primary Tools](bundled-content/tools/primary-tools.md)

## Registry

- [Overview](registry/overview.md)
- [Client Workflows](registry/client-workflows.md)

### Registry API

- [Overview](registry/registry-api/overview.md)
- [Configuration](registry/registry-api/configuration.md)
- [Local Development](registry/registry-api/local-development.md)
- [Deployment](registry/registry-api/deployment.md)
- [API Reference](registry/registry-api/api-reference.md)

## Reference

- [Permissions Reference](reference/permissions-reference.md)
- [Error Codes](reference/error-codes.md)
- [Glossary](reference/glossary.md)

### File Formats

- [Directive Format](reference/file-formats/directive-format.md)
- [Tool Metadata](reference/file-formats/tool-metadata.md)
- [Knowledge Frontmatter](reference/file-formats/knowledge-frontmatter.md)
- [Signature Format](reference/file-formats/signature-format.md)
- [Lockfile Format](reference/file-formats/lockfile-format.md)
