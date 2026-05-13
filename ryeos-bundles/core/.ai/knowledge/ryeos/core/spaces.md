---
category: ryeos/core
tags: [fundamentals, spaces, resolution]
version: "1.0.0"
description: >
  The three-tier space resolution system — project, user, and system
  (bundle) spaces, and how items are found across them.
---

# Spaces

Rye OS resolves items across three tiers: **project**, **user**, and
**system**. The first match wins.

## Project Space

```
<project_root>/.ai/
```

Project-specific items. This is where you put directives, tools, and
knowledge unique to a project. When the CLI runs from a project
directory, the project space is checked first.

Examples:
- `.ai/directives/deploy.md` — project deploy workflow
- `.ai/tools/my-helper.sh` — project-specific tool
- `.ai/knowledge/project/architecture.md` — project context for agents

Project space items are signed by the operator's local key. They are
typically tracked in git (or at least `.ai/knowledge/` is).

## User Space

```
~/.ai/
```

Cross-project personal items. Shared across all projects on the same
machine. Good for personal directives, aliases, or knowledge that
applies everywhere.

User space items are signed by the operator's local key.

## System Space

```
<bundle_root>/.ai/
```

Items installed via bundles. Bundles are signed by their author key
and registered in the daemon's trust store. System space items are
**immutable** — you cannot edit them directly. To customize, copy
to project or user space first:

```
ryeos fetch tool:ryeos/core/sign --to project
# or
ryeos fetch tool:ryeos/core/sign --to user
```

The copied item lives in project/user space and shadows the system
version (first match wins).

## Installed Bundles

Bundles are registered in the daemon's state directory:

```
<state_dir>/.ai/bundles/
├── core/       ← Always present, provides kinds/parsers/handlers/tools
├── standard/   ← Runtimes, model providers, routing
└── custom/     ← User-installed bundles
```

Use `ryeos bundle list` to see installed bundles.
Use `ryeos bundle install <path>` to add one.

## Resolution Order

For a canonical ref like `tool:ryeos/core/sign`:

```
1. <project>/.ai/tools/ryeos/core/sign.{py,yaml,js,ts,...}
2. ~/.ai/tools/ryeos/core/sign.{py,yaml,js,ts,...}
3. <bundle>/.../.ai/tools/ryeos/core/sign.{py,yaml,js,ts,...}  (for each installed bundle)
```

If a project-level item exists, it **shadows** the system version.
This allows local overrides without modifying bundles.
