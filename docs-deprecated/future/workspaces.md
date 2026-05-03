# Workspaces — The Machine-Level Grouping of Projects

> _One machine. Many projects. One workspace._

A workspace is the layer above the three-tier space system. It's not a replacement for project / user / system — it's the organizational unit that binds them together on a given machine.

Today, Rye OS resolves items within a single project context: project space → user space → system space. Each `project_path` is independent. There's no concept of "all the projects I'm working with right now" or "these three repos share the same `.ai/` tools." The machine is invisible.

A workspace makes the machine visible.

---

## What a Workspace Is

A workspace is a named collection of projects on a machine, sharing a single user space.

```
Workspace: "default"
├── campaign-kiwi/          → project space: campaign-kiwi/.ai/
├── ryeos/                  → project space: ryeos/.ai/
├── client-portal/          → project space: client-portal/.ai/
└── User Space: ~/.ai/      → shared across all three
```

Each project retains its own `.ai/` directory. Each project still resolves items through the three-tier system. The workspace is the thing that knows these projects exist together — that they're part of the same working context on this machine.

---

## Workspace Configuration

```yaml
# ~/.ai/workspaces/default.yaml
name: default
projects:
  - path: /home/leo/projects/campaign-kiwi
  - path: /home/leo/projects/ryeos
  - path: /home/leo/projects/client-portal
    shared_space: /home/leo/projects/ryeos   # uses ryeos's .ai/
```

```yaml
# ~/.ai/workspaces/consulting.yaml
name: consulting
projects:
  - path: /home/leo/consulting/acme-corp
  - path: /home/leo/consulting/globex
```

Workspaces live in user space — they're cross-project by nature. A machine can have multiple workspace definitions. One is active at a time, or tooling can infer the workspace from the current directory.

---

## Shared Project Spaces

Two projects can share the same `.ai/` directory:

```yaml
projects:
  - path: /home/leo/projects/ryeos
  - path: /home/leo/projects/ryeos-docs
    shared_space: /home/leo/projects/ryeos
```

`ryeos-docs` has no `.ai/` of its own. Its project space resolves to `ryeos/.ai/`. Both repos see the same tools, directives, and knowledge. Changes to `ryeos/.ai/` affect both.

This is useful when:
- Multiple repos form a single logical project (monorepo-adjacent)
- A docs site needs the same agent capabilities as the main codebase
- You want to author tools in one place and use them across related repos

The resolver doesn't change — `shared_space` just redirects where project space points. The three-tier precedence is untouched.

---

## Relationship to Remote

Today, `rye remote push` syncs one project at a time. Each project gets its own `project_refs` row. User space gets its own `user_space_refs` row (post step 9–10).

A workspace gives the remote layer a natural grouping:

```
rye remote push                    # push the current project
rye remote push --workspace        # push all projects in the active workspace
rye remote status --workspace      # show sync state for all projects
```

On the remote side, the workspace isn't a new database entity — it's a client-side concept that batches operations across existing `project_refs` rows. The remote doesn't need to know about workspaces. It just sees N project pushes and one user space push.

But the workspace definition itself could optionally sync to user space on the remote. If you set up a second machine and pull your user space, you get your workspace definitions — the map of which projects belong together. You still need to clone the repos, but the organizational structure travels with you.

---

## Cross-Project Execution

With a workspace, one project can reference another:

```yaml
# In campaign-kiwi/.ai/directives/deploy.md
# This directive needs to run a tool from ryeos
```

Today this is impossible — project space is scoped to one `.ai/` directory. Cross-project references would break if the other project isn't present.

Workspaces make this safe:

1. The workspace knows which projects are co-present
2. A directive can declare a cross-project dependency
3. The resolver checks the workspace definition to find the target project
4. If the target isn't in the workspace, the reference fails cleanly

This is **not** about merging project spaces. Each project's `.ai/` remains independent. Cross-project execution is an explicit, workspace-scoped capability — not implicit resolution.

```yaml
# Possible future syntax in a directive or tool
cross_project:
  requires: ryeos
  item: rye/core/registry/registry
```

The workspace validates that `ryeos` is present and resolvable. The executor resolves the item in the target project's space, not the calling project's space.

---

## User Space Is Already Workspace-Scoped

`~/.ai/` is shared across all projects on a machine. It's already the thing that makes a workspace coherent — your signing keys, your trusted authors, your global tools, your agent config.

Workspaces don't change user space. They formalize what user space already implies: that there's a machine-level context above individual projects.

---

## Multi-Machine Workspaces

The same workspace definition on two machines:

```
Machine A (laptop)                    Machine B (remote)
├── Workspace: default                ├── Workspace: default
│   ├── campaign-kiwi/                │   ├── campaign-kiwi/
│   ├── ryeos/                        │   ├── ryeos/
│   └── User Space: ~/.ai/            │   └── User Space: ~/.ai/
```

Both machines share the same user space (synced via remote). Both have the same workspace definition. Both can push and pull the same projects. The workspace is the unit of "what this machine is working on" — and when two machines share the same workspace, they're two instances of the same working context.

This connects to the fold-back model. Two machines pushing the same project create concurrent snapshots. The optimistic CAS on `snapshot_revision` handles conflicts — first push fast-forwards, second push three-way merges. The workspace doesn't change the concurrency model. It just makes it natural to have multiple machines working on the same set of projects.

---

## Relationship to Shard Space

In the [Shard Space](shard-space.md) visualization:

- **System space** is the galactic center — the kernel, the immutable core
- **User space** is the solar system — your star, your identity, your cross-project items
- **Projects** are planets orbiting your star

A workspace is the **orbital plane**. It's the slice of your solar system that you're currently looking at. Not all your planets — just the ones in this workspace's orbit. The consulting workspace shows a different set of planets than the default workspace.

Switching workspaces is rotating the orbital plane. The star stays the same. The kernel stays the same. The planets are different.

---

## Relationship to Mission Control

[Mission Control](mission-control.md) shows "the whole self at once." Today that means all remotes, all projects, all executions.

With workspaces, Mission Control gains a natural scope:

- **Workspace view**: the projects in this workspace, their sync state, their recent executions
- **All view**: everything, across all workspaces — the full solar system

Most of the time you care about the workspace view. You're working on campaign-kiwi and ryeos together. You want to see their joint state — not every project you've ever pushed.

The workspace is the default scope for Mission Control. The all-projects view is available but not the landing page.

---

## What It's Not

**Not a monorepo tool.** Workspaces don't merge `.ai/` directories or create a unified item namespace. Each project resolves independently. The workspace is organizational, not structural.

**Not a virtual environment.** The workspace doesn't isolate Python packages, system tools, or runtimes. It groups Rye OS projects — nothing else.

**Not required.** A single project with no workspace config works exactly as it does today. `project_path` resolves to the local `.ai/`, user space resolves to `~/.ai/`, system space resolves to the installed bundles. Workspaces are additive.

---

## Implementation Path

### Phase 1: Workspace Definition

Workspace YAML config in `~/.ai/workspaces/`. CLI commands to create, list, and switch workspaces. `rye workspace list`, `rye workspace create`, `rye workspace add-project`.

### Phase 2: Shared Spaces

`shared_space` support in workspace config. The resolver reads workspace config when `shared_space` is set and redirects project space resolution.

### Phase 3: Workspace-Scoped Remote Operations

`--workspace` flag on `rye remote push`, `rye remote pull`, `rye remote status`. Batch operations across all projects in the workspace.

### Phase 4: Cross-Project References

`cross_project` declarations in directives and tools. Workspace-aware resolver that can reach into other projects' spaces. Chain validator updated to allow cross-project chains within a workspace.

### Phase 5: Mission Control Integration

Workspace as the default scope in Mission Control. Workspace switcher. Per-workspace execution history and sync state.
