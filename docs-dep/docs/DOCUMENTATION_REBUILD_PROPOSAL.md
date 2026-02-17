# Documentation Rebuild Proposal

**Date:** February 10, 2026  
**Status:** Analysis Complete - Proposal Ready for Review

---

## Executive Summary

The Rye OS documentation is currently fragmented, inconsistent, and partially outdated. This proposal outlines a systematic approach to rebuilding the entire documentation system using the knowledge entry framework, ensuring consistency, discoverability, and maintainability.

---

## Current State Analysis

### Documentation Inventory

| Location                                            | Purpose               | Status                                   |
| --------------------------------------------------- | --------------------- | ---------------------------------------- |
| `/home/leo/projects/rye-os/README.md`               | Project overview      | Partially outdated                       |
| `/home/leo/projects/rye-os/RYE_OS_CONTEXT.md`       | Architecture context  | Current but needs restructuring          |
| `/home/leo/projects/rye-os/docs/`                   | General documentation | Fragmented, some outdated                |
| `/home/leo/projects/rye-os/docs/rye/`               | RYE-specific docs     | Inconsistent with current implementation |
| `/home/leo/projects/rye-os/docs/lilux/`             | Lilux docs            | Needs integration with RYE               |
| `/home/leo/projects/rye-os/docs/rye/primary-items/` | Metadata references   | Needs validation                         |
| `.ai/knowledge/`                                    | Knowledge entries     | Sparse, only metadata refs exist         |

### Critical Issues Identified

#### 1. Naming Inconsistencies

| Issue                                      | Occurrences |
| ------------------------------------------ | ----------- |
| "rye-os" vs "rye-lilux" vs "RYE" vs "rye"  | 15+         |
| "4 MCP tools" vs "5 MCP tools" (help tool) | 3 docs      |
| "kiwi-mcp" (old name) still referenced     | 5 docs      |
| "RYE-Lilux" vs "Rye OS"                    | Throughout  |

#### 2. Outdated References

- Permission format changed but old format still documented
- Tool metadata format inconsistent (YAML vs Python docstrings)
- Some code examples reference non-existent files
- Bundle structure has evolved but docs not fully updated

#### 3. Structural Problems

- Knowledge entries scattered across multiple `.ai/` directories
- No unified navigation or hierarchy
- Redundant information across documents
- Missing cross-references between related topics

#### 4. Documentation Gaps

| Gap Area                      | Impact                              |
| ----------------------------- | ----------------------------------- |
| Getting started guide         | High - new users struggle           |
| Architecture decision records | Medium - unclear why decisions made |
| Migration guides              | Medium - hard to upgrade            |
| Troubleshooting guide         | High - no central error reference   |
| Tool development tutorial     | Medium - no step-by-step guide      |

---

## Proposed Solution

### Phase 1: Foundation (Week 1)

#### 1.1 Create Core Knowledge Entries

Create foundational knowledge entries that serve as the single source of truth:

```
.ai/knowledge/rye/
├── core/
│   ├── architecture-overview.md      # System architecture
│   ├── mcp-tools-reference.md        # 4 MCP tools reference
│   ├── item-types.md                 # Directives, tools, knowledge
│   └── terminology.md                # Consistent naming guide
├── getting-started/
│   ├── quickstart.md                 # 5-minute quick start
│   ├── installation.md               # Installation guide
│   └── first-project.md              # Step-by-step tutorial
├── concepts/
│   ├── data-driven-architecture.md  # Core design principle
│   ├── tool-resolution.md            # How tools are resolved
│   ├── execution-model.md            # How execution works
│   └── permissions.md                # Security model
└── guides/
    ├── creating-directives.md        # Directive authoring
    ├── creating-tools.md             # Tool development
    └── creating-knowledge.md         # Knowledge entry authoring
```

#### 1.2 Establish Documentation Standards

**Knowledge Entry Template:**

````yaml
---
id: {kebab-case-id}
title: {Human-readable title}
category: {category/subcategory}
version: "1.0.0"
author: rye-os
tags:
  - tag1
  - tag2
  - tag3
created: 2026-02-10T00:00:00Z
validated: 2026-02-10T00:00:00Z
---

# {Title}

## Overview

{2-3 sentence summary}

## Prerequisites

- {Prerequisite 1}
- {Prerequisite 2}

## Content

{Main documentation content}

## Examples

```{language}
{code example}
````

## Related

- [Related Entry 1](./related-entry-1.md)
- [Related Entry 2](./related-entry-2.md)

## See Also

- [External Reference 1](url)
- [External Reference 2](url)

````

### Phase 2: Content Migration (Week 2)

#### 2.1 Migrate Existing Documentation

| Source Document | Target Knowledge Entry | Priority |
|----------------|----------------------|----------|
| README.md | `rye/core/architecture-overview.md` | High |
| docs/index.md | `rye/getting-started/quickstart.md` | High |
| docs/rye/principles.md | `rye/concepts/data-driven-architecture.md` | High |
| docs/rye/bundle/structure.md | `rye/core/bundle-structure.md` | High |
| docs/rye/mcp-tools/overview.md | `rye/core/mcp-tools-reference.md` | High |
| docs/rye/primary-items/* | `rye/core/item-types.md` | High |
| docs/lilux/* | `lilux/*` (new category) | Medium |
| ARCHITECTURE_SUMMARY.md | `rye/core/architecture-overview.md` (merge) | High |
| RYE_OS_CONTEXT.md | `rye/core/architecture-overview.md` (merge) | Medium |

#### 2.2 Create New Documentation

| Knowledge Entry | Purpose | Priority |
|----------------|---------|----------|
| `rye/guides/troubleshooting.md` | Common errors and solutions | High |
| `rye/guides/migration-v1.md` | Upgrade guide | Medium |
| `rye/architecture/decisions/` | ADR collection | Medium |
| `rye/security/permissions.md` | Security model deep dive | Medium |
| `lilux/primitives/overview.md` | Lilux primitives reference | Medium |

### Phase 3: Integration (Week 3)

#### 3.1 Update README.md

Replace scattered content with unified reference:

```markdown
# Rye OS

AI agent workflow portability layer.

## Quick Start

1. [Install](.ai/knowledge/rye/getting-started/installation.md)
2. [Configure](.ai/knowledge/rye/getting-started/first-project.md)
3. [Build your first directive](.ai/knowledge/rye/guides/creating-directives.md)

## Documentation

- [Architecture](.ai/knowledge/rye/core/architecture-overview.md)
- [MCP Tools Reference](.ai/knowledge/rye/core/mcp-tools-reference.md)
- [API Reference](.ai/knowledge/rye/core/api-reference.md)
- [Guides](.ai/knowledge/rye/guides/)
- [Troubleshooting](.ai/knowledge/rye/guides/troubleshooting.md)

## Architecture

Rye OS provides:
- 4 MCP tools for agent integration
- 3 data-driven item types
- Portable workflows across AI agents

## Contributing

See [Contributing Guide](.ai/knowledge/rye/guides/contributing.md).
````

#### 3.2 Create Documentation Index

Create `.ai/knowledge/INDEX.md` that serves as the documentation entry point:

```markdown
# Rye OS Documentation Index

## Getting Started

- [Quick Start](getting-started/quickstart.md)
- [Installation](getting-started/installation.md)
- [First Project](getting-started/first-project.md)

## Core Concepts

- [Architecture Overview](core/architecture-overview.md)
- [MCP Tools Reference](core/mcp-tools-reference.md)
- [Item Types](core/item-types.md)
- [Data-Driven Architecture](core/data-driven-architecture.md)
- [Tool Resolution](core/tool-resolution.md)
- [Execution Model](core/execution-model.md)
- [Permissions](core/permissions.md)

## Guides

- [Creating Directives](guides/creating-directives.md)
- [Creating Tools](guides/creating-tools.md)
- [Creating Knowledge](guides/creating-knowledge.md)
- [Troubleshooting](guides/troubleshooting.md)

## Lilux Documentation

- [Lilux Overview](../lilux/overview.md)
- [Primitives](../lilux/primitives/overview.md)
- [Runtime Services](../lilux/runtime-services/overview.md)

## Architecture Decisions

- [ADR Index](../architecture/decisions/adr-000-index.md)
```

### Phase 4: Validation (Week 4)

#### 4.1 Consistency Checks

Create validation script to ensure:

- All knowledge entries have valid YAML frontmatter
- All internal links point to existing entries
- No outdated terminology (kiwi-mcp, etc.)
- Consistent naming conventions
- All examples compile/run correctly

#### 4.2 Review Process

1. **Self-review**: Ensure each entry follows standards
2. **Peer review**: Cross-reference with implementation
3. **Integration test**: Verify links work and examples run
4. **Validation**: Sign all entries using rye_sign

---

## Knowledge Entry Mapping

### Tier 1: Must Have (Week 1)

| ID                         | Title                        | Category            | Content Summary                        |
| -------------------------- | ---------------------------- | ------------------- | -------------------------------------- |
| `architecture-overview`    | Rye OS Architecture Overview | rye/core            | System architecture, layers, data flow |
| `mcp-tools-reference`      | MCP Tools Reference          | rye/core            | Complete reference for 4 MCP tools     |
| `terminology`              | Terminology and Naming       | rye/core            | Consistent terminology guide           |
| `quickstart`               | Quick Start Guide            | rye/getting-started | 5-minute getting started               |
| `installation`             | Installation Guide           | rye/getting-started | Detailed installation                  |
| `first-project`            | Your First Project           | rye/getting-started | Step-by-step tutorial                  |
| `data-driven-architecture` | Data-Driven Architecture     | rye/concepts        | Core design principles                 |
| `bundle-structure`         | Bundle Structure             | rye/core            | .ai/ directory organization            |
| `tool-resolution`          | Tool Resolution              | rye/concepts        | How tools are resolved and loaded      |
| `creating-directives`      | Creating Directives          | rye/guides          | Directive authoring guide              |
| `creating-tools`           | Creating Tools               | rye/guides          | Tool development guide                 |
| `creating-knowledge`       | Creating Knowledge           | rye/guides          | Knowledge entry authoring              |

### Tier 2: Should Have (Week 2)

| ID                    | Title                 | Category         | Content Summary                    |
| --------------------- | --------------------- | ---------------- | ---------------------------------- |
| `execution-model`     | Execution Model       | rye/concepts     | How tools and directives execute   |
| `permissions`         | Permissions Model     | rye/concepts     | Security model documentation       |
| `item-types`          | Item Types Reference  | rye/core         | Directives, tools, knowledge specs |
| `troubleshooting`     | Troubleshooting Guide | rye/guides       | Common errors and solutions        |
| `migration-v1`        | Migration Guide v1.x  | rye/guides       | Upgrading from previous versions   |
| `lilux-overview`      | Lilux Overview        | lilux            | Microkernel documentation          |
| `primitives-overview` | Lilux Primitives      | lilux/primitives | Subprocess, HTTP, primitives       |
| `security-model`      | Security Model        | rye/security     | Deep dive on permissions           |

### Tier 3: Nice to Have (Week 3-4)

| ID                       | Title                         | Category         | Content Summary              |
| ------------------------ | ----------------------------- | ---------------- | ---------------------------- |
| `contributing`           | Contributing Guide            | rye/guides       | How to contribute            |
| `architecture-decisions` | Architecture Decision Records | rye/architecture | Why decisions were made      |
| `performance`            | Performance Guide             | rye/guides       | Optimization tips            |
| `testing`                | Testing Guide                 | rye/guides       | Testing directives and tools |
| `deployment`             | Deployment Patterns           | rye/guides       | Production deployment        |
| `registry-guide`         | Registry Guide                | rye/guides       | Publishing and sharing       |
| `agent-threads`          | Agent Threads                 | rye/concepts     | Thread management            |
| `capabilities`           | Capabilities Reference        | rye/concepts     | Sandboxing and capabilities  |
| `telemetry`              | Telemetry Guide               | rye/guides       | Monitoring and observability |
| `error-codes`            | Error Code Reference          | rye/reference    | Complete error code list     |

---

## Implementation Checklist

### Week 1: Foundation

- [ ] Create knowledge entry template
- [ ] Create terminology guide
- [ ] Create architecture overview
- [ ] Create MCP tools reference
- [ ] Create quickstart guide
- [ ] Create installation guide
- [ ] Create first project tutorial
- [ ] Create bundle structure doc
- [ ] Create tool resolution doc
- [ ] Create data-driven architecture doc
- [ ] Create creating-directives guide
- [ ] Create creating-tools guide
- [ ] Create creating-knowledge guide
- [ ] Sign all Tier 1 entries

### Week 2: Content Migration

- [ ] Migrate README.md content
- [ ] Migrate ARCHITECTURE_SUMMARY.md
- [ ] Migrate RYE_OS_CONTEXT.md
- [ ] Migrate docs/index.md
- [ ] Migrate docs/rye/principles.md
- [ ] Migrate docs/rye/bundle/structure.md
- [ ] Migrate docs/rye/mcp-tools/overview.md
- [ ] Create execution model doc
- [ ] Create permissions doc
- [ ] Create item types reference
- [ ] Create troubleshooting guide
- [ ] Create migration guide
- [ ] Create Lilux overview
- [ ] Create Lilux primitives doc
- [ ] Sign all Tier 2 entries

### Week 3: Integration

- [ ] Update main README.md
- [ ] Create documentation index
- [ ] Add cross-references between entries
- [ ] Verify all links work
- [ ] Update docs/index.md to point to knowledge entries
- [ ] Remove redundant documentation
- [ ] Archive outdated docs

### Week 4: Validation

- [ ] Create validation script
- [ ] Check all YAML frontmatter
- [ ] Verify internal links
- [ ] Check terminology consistency
- [ ] Run all code examples
- [ ] Peer review of all entries
- [ ] Final sign all entries
- [ ] Update documentation index

---

## Rollout Plan

### Pre-Rollout

1. **Backup current docs**: Create `docs/legacy/` directory
2. **Create new structure**: Set up knowledge entry hierarchy
3. **Validate implementation**: Ensure rye_sign works correctly

### Rollout Steps

1. **Phase 1**: Deploy Tier 1 entries
2. **Phase 2**: Deploy Tier 2 entries
3. **Phase 3**: Update navigation and cross-references
4. **Phase 4**: Deprecate legacy docs (move to `docs/legacy/`)

### Post-Rollout

1. **Monitor**: Track documentation issues
2. **Iterate**: Fix gaps based on user feedback
3. **Maintain**: Add new entries as system evolves
4. **Review**: Quarterly documentation audit

---

## Success Metrics

| Metric                  | Target                       | Measurement                          |
| ----------------------- | ---------------------------- | ------------------------------------ |
| Documentation coverage  | 100% of features             | All features documented              |
| Link consistency        | 0 broken links               | Automated validation                 |
| Terminology consistency | 0 violations                 | Automated checks                     |
| User satisfaction       | >90% satisfied               | Survey                               |
| Time to productivity    | <30 minutes                  | Time from install to first directive |
| Issue reduction         | 50% fewer doc-related issues | Support tickets                      |

---

## Risks and Mitigations

| Risk                             | Impact | Mitigation                                  |
| -------------------------------- | ------ | ------------------------------------------- |
| Documentation drift              | High   | Automated validation, regular reviews       |
| User confusion during transition | Medium | Clear migration path, legacy docs available |
| Incomplete coverage              | Medium | Phased approach, prioritization             |
| Tooling issues                   | Low    | Backup validation methods                   |

---

## Budget Estimate

| Item                  | Time        | Effort        |
| --------------------- | ----------- | ------------- |
| Analysis and planning | Week 1      | 10 hours      |
| Tier 1 creation       | Week 1      | 20 hours      |
| Tier 2 creation       | Week 2      | 25 hours      |
| Migration             | Week 2      | 15 hours      |
| Integration           | Week 3      | 15 hours      |
| Validation            | Week 4      | 10 hours      |
| **Total**             | **4 weeks** | **~95 hours** |

---

## Next Steps

1. **Review**: Team reviews this proposal
2. **Approve**: Sign off on approach
3. **Initialize**: Create knowledge entry template in `.ai/knowledge/rye/core/`
4. **Begin**: Start with Tier 1 entries
5. **Iterate**: Weekly check-ins to track progress

---

## Appendix

### A. Knowledge Entry Format Reference

See `/home/leo/projects/rye-os/rye/rye/.ai/directives/rye/core/create_knowledge.md`

### B. Existing Documentation Sources

- `/home/leo/projects/rye-os/README.md` (782 lines)
- `/home/leo/projects/rye-os/RYE_OS_CONTEXT.md` (1173 lines)
- `/home/leo/projects/rye-os/docs/` (90+ markdown files)
- `/home/leo/projects/rye-os/docs/rye/` (50+ files)
- `/home/leo/projects/rye-os/docs/lilux/` (20+ files)

### C. Related Projects

- Rye OS MCP server implementation
- Lilux microkernel
- Knowledge entry signing system
