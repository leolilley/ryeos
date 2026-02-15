# Tool Anchor System

How runtime YAMLs declare import resolution, dependency verification scope, and execution environment for multi-file tools — replacing hardcoded language-specific loaders with a data-driven pattern.

## The Problem

Tools under `.ai/tools/` often consist of multiple files. The thread system is the extreme case:

```
.ai/tools/rye/agent/threads/
├── adapters/
│   └── tool_dispatcher.py          ← from module_loader import load_module
├── events/
│   └── event_emitter.py            ← from module_loader import load_module
├── loaders/
│   ├── config_loader.py
│   ├── condition_evaluator.py
│   ├── hooks_loader.py
│   └── resilience_loader.py
├── persistence/
│   ├── transcript.py
│   ├── thread_registry.py
│   └── budgets.py
├── module_loader.py                ← tool-local sys.modules helper
├── thread_directive.py             ← entry point (subprocess)
├── runner.py
├── safety_harness.py
└── orchestrator.py
```

Three problems exist today:

1. **sys.path gap**: When `python_script_runtime` spawns `thread_directive.py` via subprocess, the tool's directory is not on `PYTHONPATH`. The import `from module_loader import load_module` fails because the subprocess only sees installed packages and site-packages.

2. **Verification gap**: `PrimitiveExecutor` calls `verify_item()` on every chain element (the tool file itself + its runtime YAML + the primitive). But files loaded dynamically at runtime — `safety_harness.py`, `loaders/hooks_loader.py`, etc. — are not chain elements and bypass verification entirely. A tampered `safety_harness.py` would execute unchecked.

3. **Language coupling**: The current fix (`module_loader.py`) is Python-specific. Node tools would need `NODE_PATH` injection, shell tools would need `PATH` additions. There's no unified pattern.

## Design Decision

The runtime YAML is the right place for anchor semantics. Runtimes already own interpreter resolution, environment variables, and command templates — they define "how to set up a language's execution environment." Adding anchor config there keeps the pattern consistent: the runtime knows its language.

No new files are introduced. No per-tool config. The anchor is a runtime-level concern — it applies uniformly to all tools using that runtime, with auto-detection to avoid breaking simple single-file tools.

## YAML Schema

Two new top-level sections on runtime YAMLs: `anchor` and `verify_deps`.

### `anchor` — Import Resolution Setup

```yaml
anchor:
  enabled: true

  # When to apply anchor behavior
  #   auto:   only when marker files exist under the tool's directory
  #   always: every tool using this runtime gets anchor setup
  #   never:  disabled (anchor section ignored)
  mode: auto

  # Marker files that trigger "auto" mode.
  # If ANY marker exists under tool_dir, anchor applies.
  markers_any: ["__init__.py", "pyproject.toml"]

  # Base directory for anchor resolution
  #   tool_dir:    tool_path.parent (most common)
  #   tool_parent: tool_path.parent.parent
  #   project_path: self.project_path
  root: tool_dir

  # Optional: runtime library path, relative to this YAML's directory.
  # Resolved as {runtime_lib} for use in env_paths.
  lib: lib/python

  # Optional: set cwd for the subprocess (null = don't change)
  cwd: null

  # Path-like environment variable mutations
  # Each var gets prepend/append lists, joined with os.pathsep
  env_paths:
    PYTHONPATH:
      prepend: ["{anchor_path}"]
```

### `verify_deps` — Pre-Spawn Dependency Verification

```yaml
verify_deps:
  enabled: true

  # Verification scope
  #   anchor:        walk anchor_path/** (recursive)
  #   tool_dir:      walk tool_dir/** (recursive)
  #   tool_siblings: walk tool_dir/* (non-recursive)
  #   tool_file:     verify only the tool entry point
  scope: anchor

  recursive: true

  # File extensions to verify (others ignored)
  extensions: [".py", ".yaml", ".yml", ".json"]

  # Directories to skip during walk
  exclude_dirs: ["__pycache__", ".venv", "node_modules", ".git"]
```

### Template Variables

Both sections support `{var}` template substitution with these variables:

| Variable         | Value                                              |
| ---------------- | -------------------------------------------------- |
| `{tool_path}`    | Absolute path to the tool entry point              |
| `{anchor_path}`  | Resolved anchor root directory                     |
| `{tool_dir}`     | `tool_path.parent`                                 |
| `{tool_parent}`  | `tool_path.parent.parent`                          |
| `{runtime_lib}`  | Runtime's `lib/{language}/` directory (co-located) |
| `{project_path}` | Project root                                       |
| `{user_space}`   | User space path (`~/.ai/`)                         |
| `{system_space}` | System space path (site-packages)                  |

## Runtime YAML Updates

### `python_script_runtime.yaml`

```yaml
version: "1.0.0"
tool_type: runtime
executor_id: rye/core/primitives/subprocess
category: rye/core/runtimes
description: "Python script runtime"

env_config:
  interpreter:
    type: venv_python
    venv_path: .venv
    var: RYE_PYTHON
    fallback: python3
  env:
    PYTHONUNBUFFERED: "1"
    PROJECT_VENV_PYTHON: "${RYE_PYTHON}"

anchor:
  enabled: true
  mode: auto
  markers_any: ["__init__.py", "pyproject.toml"]
  root: tool_dir
  lib: lib/python
  env_paths:
    PYTHONPATH:
      prepend: ["{anchor_path}", "{runtime_lib}"]

verify_deps:
  enabled: true
  scope: anchor
  recursive: true
  extensions: [".py", ".yaml", ".yml", ".json"]
  exclude_dirs: ["__pycache__", ".venv", "node_modules", ".git"]

config:
  command: "${RYE_PYTHON}"
  args:
    - "{tool_path}"
    - "--params"
    - "{params_json}"
    - "--project-path"
    - "{project_path}"
  timeout: 300
```

### `node_runtime.yaml`

```yaml
version: "1.0.0"
tool_type: runtime
executor_id: rye/core/primitives/subprocess
category: rye/core/runtimes
description: "Node.js runtime executor"

env_config:
  interpreter:
    type: node_modules
    search_paths: [node_modules/.bin]
    var: RYE_NODE
    fallback: node
  env:
    NODE_ENV: development

anchor:
  enabled: true
  mode: auto
  markers_any: ["package.json"]
  root: tool_dir
  lib: lib/node
  cwd: "{anchor_path}"
  env_paths:
    NODE_PATH:
      prepend: ["{anchor_path}", "{anchor_path}/node_modules"]

verify_deps:
  enabled: true
  scope: anchor
  recursive: true
  extensions: [".js", ".ts", ".mjs", ".cjs", ".json", ".yaml", ".yml"]
  exclude_dirs: ["node_modules", "__pycache__", ".git", "dist", "build"]

config:
  command: "${RYE_NODE}"
  args: []
  timeout: 300
```

### `shell_runtime.yaml` (future)

```yaml
anchor:
  enabled: true
  mode: always
  root: tool_dir
  env_paths:
    PATH:
      prepend: ["{anchor_path}/bin", "{anchor_path}"]

verify_deps:
  enabled: true
  scope: tool_dir
  extensions: [".sh", ".bash", ".yaml", ".yml", ".json"]
  exclude_dirs: [".git"]
```

## PrimitiveExecutor Changes

### Execution Flow (Updated)

The anchor system adds two steps between chain validation and subprocess spawn:

```
1. Build chain                              (existing)
2. Verify chain element signatures          (existing — verify_item per element)
3. Validate chain                           (existing)
4. Compute anchor context + decide if anchor applies    ← NEW
5. Verify dependencies (walk + verify_item per file)    ← NEW
6. Resolve environment                      (existing)
7. Apply anchor mutations to env + config               ← NEW (merged into step 6)
8. Execute via primitive                    (existing)
```

### New Data on ChainElement

```python
@dataclass
class ChainElement:
    """Element in the executor chain."""
    item_id: str
    path: Path
    space: str
    tool_type: Optional[str] = None
    executor_id: Optional[str] = None
    env_config: Optional[Dict[str, Any]] = None
    config_schema: Optional[Dict[str, Any]] = None
    config: Optional[Dict[str, Any]] = None
    anchor_config: Optional[Dict[str, Any]] = None       # ← NEW
    verify_deps_config: Optional[Dict[str, Any]] = None   # ← NEW
```

These are populated from the runtime YAML during `_build_chain()`, alongside existing fields.

### New Methods

```python
def _compute_anchor_context(self, chain: List[ChainElement]) -> Dict[str, str]:
    """Compute template variables for anchor resolution."""
    tool_element = chain[0]
    tool_dir = tool_element.path.parent

    # Resolve runtime_lib from the anchor config's lib field
    runtime_lib = ""
    for element in chain:
        if element.anchor_config and element.anchor_config.get("lib"):
            # lib is a relative path from the runtime YAML's directory
            runtime_lib = str(element.path.parent / element.anchor_config["lib"])
            break

    return {
        "tool_path": str(tool_element.path),
        "tool_dir": str(tool_dir),
        "tool_parent": str(tool_dir.parent),
        "anchor_path": str(tool_dir),  # default, overridden by root
        "runtime_lib": runtime_lib,
        "project_path": str(self.project_path),
        "user_space": str(self.user_space),
        "system_space": str(self.system_space),
    }

def _anchor_applies(
    self, anchor_cfg: Dict[str, Any], tool_dir: Path
) -> bool:
    """Decide whether anchor setup should activate."""
    mode = anchor_cfg.get("mode", "auto")
    if mode == "never" or not anchor_cfg.get("enabled", False):
        return False
    if mode == "always":
        return True
    # mode == "auto": check for marker files
    markers = anchor_cfg.get("markers_any", [])
    return any((tool_dir / marker).exists() for marker in markers)

def _resolve_anchor_path(
    self, anchor_cfg: Dict[str, Any], ctx: Dict[str, str]
) -> Path:
    """Resolve the anchor root directory from config."""
    root = anchor_cfg.get("root", "tool_dir")
    if root == "tool_dir":
        return Path(ctx["tool_dir"])
    elif root == "tool_parent":
        return Path(ctx["tool_parent"])
    elif root == "project_path":
        return Path(ctx["project_path"])
    return Path(ctx["tool_dir"])

def _apply_anchor_env(
    self,
    anchor_cfg: Dict[str, Any],
    resolved_env: Dict[str, str],
    ctx: Dict[str, str],
) -> None:
    """Mutate resolved_env with anchor path additions.

    Prepends/appends to path-like env vars using os.pathsep.
    Modifies resolved_env in place.
    """
    import os

    env_paths = anchor_cfg.get("env_paths", {})
    for var_name, mutations in env_paths.items():
        existing = resolved_env.get(var_name, os.environ.get(var_name, ""))
        parts = existing.split(os.pathsep) if existing else []

        for path_template in mutations.get("prepend", []):
            resolved = self._template_string(path_template, ctx)
            if resolved not in parts:
                parts.insert(0, resolved)

        for path_template in mutations.get("append", []):
            resolved = self._template_string(path_template, ctx)
            if resolved not in parts:
                parts.append(resolved)

        resolved_env[var_name] = os.pathsep.join(p for p in parts if p)

    # Apply cwd if specified
    cwd = anchor_cfg.get("cwd")
    if cwd:
        # cwd is applied later in config, not env
        pass

def _verify_tool_dependencies(
    self, chain: List[ChainElement], anchor_path: Path
) -> None:
    """Verify all files in the tool's dependency scope.

    Walks the anchor directory tree, verifying every file that
    matches the configured extensions. Runs BEFORE subprocess spawn.

    Raises IntegrityError if any file fails verification.
    """
    # Find verify_deps config from chain (runtime element)
    verify_cfg = None
    for element in chain:
        if element.verify_deps_config:
            verify_cfg = element.verify_deps_config
            break

    if not verify_cfg or not verify_cfg.get("enabled", False):
        return

    extensions = set(verify_cfg.get("extensions", []))
    exclude_dirs = set(verify_cfg.get("exclude_dirs", [
        "__pycache__", ".venv", "node_modules", ".git",
    ]))
    recursive = verify_cfg.get("recursive", True)

    # Determine base path from scope
    scope = verify_cfg.get("scope", "anchor")
    if scope == "tool_file":
        # Only the entry point — already verified in chain
        return
    elif scope == "tool_siblings":
        base = chain[0].path.parent
        recursive = False
    elif scope == "tool_dir":
        base = chain[0].path.parent
    else:  # "anchor"
        base = anchor_path

    base = base.resolve()

    # Walk and verify
    import os
    for dirpath, dirnames, filenames in os.walk(base, followlinks=False):
        # Prune excluded directories
        dirnames[:] = [
            d for d in dirnames if d not in exclude_dirs
        ]

        if not recursive and Path(dirpath) != base:
            dirnames.clear()
            continue

        for filename in filenames:
            filepath = Path(dirpath) / filename
            if filepath.suffix not in extensions:
                continue

            # Guard against symlink escapes
            real = filepath.resolve()
            if not str(real).startswith(str(base)):
                raise IntegrityError(
                    f"Symlink escape: {filepath} resolves to {real}"
                )

            verify_item(
                filepath, ItemType.TOOL,
                project_path=self.project_path,
            )
```

### Integration Point in `execute()`

```python
async def execute(self, item_id, parameters=None, ...):
    # ... existing steps 1-4 ...

    # 4.5 Anchor + verify_deps (NEW)
    anchor_cfg = None
    for element in chain:
        if element.anchor_config:
            anchor_cfg = element.anchor_config
            break

    anchor_ctx = self._compute_anchor_context(chain)
    anchor_active = False

    if anchor_cfg and self._anchor_applies(anchor_cfg, chain[0].path.parent):
        anchor_active = True
        anchor_path = self._resolve_anchor_path(anchor_cfg, anchor_ctx)
        anchor_ctx["anchor_path"] = str(anchor_path)

        # Verify all dependencies BEFORE spawn
        self._verify_tool_dependencies(chain, anchor_path)

    # 5. Resolve environment through the chain (existing)
    resolved_env = self._resolve_chain_env(chain)

    # 5.5 Apply anchor env mutations (NEW)
    if anchor_active:
        self._apply_anchor_env(anchor_cfg, resolved_env, anchor_ctx)

    # 6. Execute via the root primitive (existing)
    # Pass cwd from anchor if configured
    if anchor_active and anchor_cfg.get("cwd"):
        cwd = self._template_string(anchor_cfg["cwd"], anchor_ctx)
        parameters = {**parameters, "cwd": cwd}

    result = await self._execute_chain(chain, parameters, resolved_env)
    # ...
```

## How It Solves Each Problem

### Problem 1: sys.path gap

When `python_script_runtime` spawns `thread_directive.py`:

1. Anchor detects `module_loader.py` marker in `threads/` directory → anchor applies
2. `PYTHONPATH` gets `threads/` prepended
3. Subprocess inherits `PYTHONPATH` via env merge
4. `from module_loader import load_module` resolves from `PYTHONPATH`

No code changes needed in tool files. The bare import `from module_loader import load_module` works because Python searches `PYTHONPATH` before site-packages.

### Problem 2: Verification gap

Before the subprocess spawns:

1. `_verify_tool_dependencies()` walks `threads/` recursively
2. Every `.py`, `.yaml`, `.yml`, `.json` file gets `verify_item()` called
3. `safety_harness.py`, `loaders/hooks_loader.py`, etc. are all verified
4. If any file is unsigned or tampered, `IntegrityError` is raised and execution aborts

The verification happens at the PrimitiveExecutor level — before Python starts. No importlib hooks needed.

### Problem 3: Language coupling

Each runtime YAML declares its own anchor semantics:

| Runtime | Path Var     | Markers                         |
| ------- | ------------ | ------------------------------- |
| Python  | `PYTHONPATH` | `__init__.py`, `pyproject.toml` |
| Node    | `NODE_PATH`  | `package.json`                  |
| Shell   | `PATH`       | (always)                        |

The PrimitiveExecutor logic is language-agnostic — it reads `env_paths` from config and prepends/appends strings to environment variables. The runtime YAML knows which variable matters for its language.

## Runtime Libraries

### The Problem with Tool-Local module_loader.py

`module_loader.py` currently lives at `.ai/tools/rye/agent/threads/module_loader.py` — a tool-local copy that only the thread system can use. But any multi-file Python tool needs `sys.modules` registration for relative imports. The bundler, registry, or any future multi-file tool would need to copy `module_loader.py` into its own directory. That's duplication, not reuse.

The same pattern applies across languages: Node tools might need a `require` helper that resolves from the tool's directory. Shell tools might need a sourcing helper.

### Runtime Library Directory

Each runtime can ship a `lib/` directory containing shared helpers that the runtime makes available to all tools via `PYTHONPATH` / `NODE_PATH` / `PATH`:

```
.ai/tools/rye/core/runtimes/
├── python_script_runtime.yaml
├── python_function_runtime.yaml
├── node_runtime.yaml
└── lib/
    ├── python/
    │   └── module_loader.py        ← moved from threads/
    └── node/
        └── (future: require helpers, etc.)
```

This mirrors how language runtimes work in the real world — Python has its stdlib on `sys.path`, Node has core modules. The runtime library is the "stdlib" for tools using that runtime.

### How the Runtime YAML References Its Library

The runtime YAML declares its lib path via the `lib` field on the `anchor` config — a relative path from the YAML's own directory. The `{runtime_lib}` template variable resolves to this path at execution time:

```yaml
# python_script_runtime.yaml
anchor:
  enabled: true
  mode: auto
  markers_any: ["__init__.py", "pyproject.toml"]
  root: tool_dir
  lib: lib/python # ← declares its own lib path
  env_paths:
    PYTHONPATH:
      prepend: ["{anchor_path}", "{runtime_lib}"] # ← used via template variable
```

No language detection logic in PrimitiveExecutor. Each runtime declares its own `lib` path — Python says `lib/python`, Node says `lib/node`, a future Rust runtime could say `lib/rust`. The executor just resolves `anchor_config["lib"]` relative to the runtime YAML's directory.

### Template Variable Resolution

`{runtime_lib}` is computed in `_compute_anchor_context()` (see [New Methods](#new-methods)) by finding the first chain element with an `anchor_config.lib` field and resolving it relative to that element's directory. Since the element's `path` is an absolute path already resolved from the correct space, `{runtime_lib}` automatically points to the right location without any space-awareness logic.

### What Changes for Thread System Files

The `from module_loader import load_module` imports in all 6 thread files stay exactly the same — they still do a bare `import module_loader`. What changes is **how** Python finds `module_loader`:

**Before (broken)**: `module_loader.py` is in `threads/` but `threads/` isn't on `PYTHONPATH`.

**After**: `module_loader.py` is in `runtimes/lib/python/`. The anchor prepends both `{anchor_path}` (the tool's directory) and `{runtime_lib}` (the runtime's lib directory) to `PYTHONPATH`. Python finds `module_loader` from `{runtime_lib}`.

The anchor auto-detection markers change too. Since `module_loader.py` is no longer in the tool's directory, the markers should be things that indicate "this is a multi-file tool": `__init__.py`, `pyproject.toml`, or the presence of subdirectories with `.py` files. For the thread system, `__init__.py` already exists.

But actually — since `module_loader` is now available via `{runtime_lib}` on PYTHONPATH regardless of anchor activation, the marker check only controls whether `{anchor_path}` (the tool's own directory) is also added. For single-file tools, the tool's directory doesn't need to be on PYTHONPATH because there are no sibling imports. For multi-file tools, it does. So markers still serve a purpose: controlling `{anchor_path}` injection and `verify_deps` scope.

### What Moves, What Stays

| File               | From                           | To                                        |
| ------------------ | ------------------------------ | ----------------------------------------- |
| `module_loader.py` | `.ai/tools/rye/agent/threads/` | `.ai/tools/rye/core/runtimes/lib/python/` |

Thread-specific files (`runner.py`, `safety_harness.py`, `loaders/`, etc.) stay in `threads/`. They are tool-specific, not runtime-shared.

### Runtime Library Verification

Files under `runtimes/lib/` are signed and verified like any other file under `.ai/tools/`. They're in the system space (shipped with the package), so they get inline `rye:signed:` signatures.

The `verify_deps` scope for tools that use runtime libraries: the runtime lib directory is verified as part of chain verification (the runtime YAML element is already verified, and `verify_deps` can be extended to cover its `lib/` directory). Alternatively, since the runtime lib files rarely change (they ship with the package), they can be covered by the system-space lockfile.

## What Doesn't Change

- **Chain verification** still verifies chain elements (tool + runtime + primitive). The anchor adds sibling verification on top.

- **Signing** is unchanged. All files under `.ai/tools/` must be signed. The anchor system enforces that before execution.

- **Single-file tools** are unaffected. `mode: auto` checks for markers — a lone `git.py` in `.ai/tools/rye/core/git/` has no `__init__.py` or `pyproject.toml`, so anchor doesn't activate for `{anchor_path}`. The runtime lib path is still available on PYTHONPATH for `module_loader` access, but single-file tools don't use it.

## Interaction with Bundle Manifests

The anchor system (Layer 1 — inline signatures) and the bundle manifest system (Layer 2 — SHA256 hashes) are complementary:

| Concern              | Anchor `verify_deps`          | Bundle Manifest         |
| -------------------- | ----------------------------- | ----------------------- |
| **What it verifies** | Inline `rye:signed:` sigs     | SHA256 content hashes   |
| **When it runs**     | Pre-spawn (PrimitiveExecutor) | Tool load time          |
| **Scope**            | Tool directory tree           | Entire bundle tree      |
| **File types**       | Code files with comment sigs  | Any file type           |
| **Who creates it**   | `rye_sign` per file           | Bundler `action=create` |

For bundled apps, both layers apply. `verify_deps` catches code tampering at execution time. The manifest catches asset tampering at load time. Belt and suspenders.

## Performance Considerations

- **Auto mode** avoids unnecessary work: single-file tools skip the directory walk entirely.
- **Extension filtering** limits verification to code files, skipping binary assets.
- **Exclude dirs** skips `__pycache__/`, `.venv/`, `node_modules/` which can be large.
- **Caching**: `verify_item()` results can be cached by `(realpath, content_hash)` in PrimitiveExecutor, similar to existing metadata caching. Files verified once per content hash don't need re-verification.

For the threads directory (~30 files), verification adds ~50ms. For a bundled app with hundreds of files, the cache makes repeat executions near-free.

## Lockfile Integration

### Using Lockfiles for Dependency Verification Cache

The existing lockfile system already stores integrity hashes for resolved chains. The `verify_deps` directory walk is essentially computing the same data — a list of `(path, integrity_hash)` pairs — but discarding it after verification. Instead, we should extend the lockfile to cache dependency hashes, turning repeat executions from O(n) directory walks into O(1) hash comparisons.

#### Extended Lockfile Schema

The `resolved_chain` field already stores chain elements with integrity hashes. Add a `verified_deps` field alongside it:

```json
{
  "lockfile_version": 2,
  "generated_at": "2026-02-14T...",
  "root": {
    "tool_id": "rye/agent/threads/thread_directive",
    "version": "1.0.0",
    "integrity": "c8cf01e2..."
  },
  "resolved_chain": [
    {
      "item_id": "rye/agent/threads/thread_directive",
      "space": "project",
      "integrity": "c8cf01e2..."
    },
    {
      "item_id": "rye/core/runtimes/python_script_runtime",
      "space": "system",
      "integrity": "a5b13b5a..."
    },
    {
      "item_id": "rye/core/primitives/subprocess",
      "space": "system",
      "integrity": null
    }
  ],
  "verified_deps": {
    "anchor_path": ".ai/tools/rye/agent/threads",
    "scope": "anchor",
    "files": {
      "module_loader.py": "sha256:abcd1234...",
      "runner.py": "sha256:ef567890...",
      "safety_harness.py": "sha256:1a2b3c4d...",
      "orchestrator.py": "sha256:5e6f7a8b...",
      "loaders/hooks_loader.py": "sha256:9c0d1e2f...",
      "loaders/config_loader.py": "sha256:aabbccdd...",
      "adapters/tool_dispatcher.py": "sha256:eeff0011..."
    }
  }
}
```

#### Verification Flow with Lockfile

```
1. Check for existing lockfile (existing step)
2. If lockfile exists AND has verified_deps:
   a. For each file in verified_deps.files:
      - Compute current SHA256
      - Compare to lockfile hash
      - If ANY mismatch → integrity error (re-sign + delete lockfile)
   b. Skip the full directory walk + verify_item() calls
   c. Proceed to execution
3. If no lockfile OR lockfile lacks verified_deps:
   a. Full directory walk + verify_item() per file (existing verify_deps logic)
   b. Collect all (relative_path, sha256) pairs
   c. Store in lockfile as verified_deps (on successful execution)
```

This means the first execution of a multi-file tool does the full walk, but every subsequent execution (with unchanged files) is a fast hash comparison — no signature verification, no directory traversal.

#### Integration with `PrimitiveExecutor.execute()`

The lockfile is already checked early in `execute()` (step 1) and created on success (step 7). The anchor system hooks into both:

```python
# In execute(), after lockfile check (step 1):
if lockfile and lockfile.verified_deps:
    # Fast path: compare hashes only
    self._verify_deps_from_lockfile(lockfile.verified_deps, chain[0].path)
    # Skip full verify_deps walk later

# After successful execution (step 7), when creating lockfile:
if anchor_active:
    dep_hashes = self._collect_dep_hashes(anchor_path, verify_cfg)
    # Include in lockfile creation
    new_lockfile = self.lockfile_resolver.create_lockfile(
        tool_id=item_id,
        version=version,
        integrity=integrity,
        resolved_chain=resolved_chain,
        verified_deps=dep_hashes,  # NEW field
    )
```

### Lockfile Scope: Follow the Tool's Resolved Space

Currently `LockfileResolver` defaults to `scope="user"` — always writing lockfiles to `~/.ai/lockfiles/`. This is wrong. The lockfile captures a chain resolved in a specific space context, so the lockfile should live alongside the tool that produced it.

The chain already knows where each tool was found — `ChainElement.space` is `"project"`, `"user"`, or `"system"`. The lockfile write location should follow the root tool's space:

| Root tool space | Lockfile write path             | Why                                           |
| --------------- | ------------------------------- | --------------------------------------------- |
| `project`       | `{project_path}/.ai/lockfiles/` | Tool is project-local, lockfile should be too |
| `user`          | `~/.ai/lockfiles/`              | Tool is user-global                           |
| `system`        | `~/.ai/lockfiles/`              | System is read-only, fall back to user        |

**Change**: Remove the static `scope` parameter. Instead, `save_lockfile` accepts the resolved space and routes accordingly.

```python
class LockfileResolver:
    def save_lockfile(self, lockfile: Lockfile, space: str = "project") -> Path:
        """Save lockfile to the appropriate location based on resolved space.

        Args:
            lockfile: Lockfile to save
            space: Space where the root tool was resolved ("project", "user", "system")
        """
        path = self._resolve_write_path(
            lockfile.root.tool_id,
            lockfile.root.version,
            space,
        )
        ensure_parent_directory(path)
        return self.manager.save(lockfile, path)

    def _resolve_write_path(self, tool_id: str, version: str, space: str) -> Path:
        """Determine write location from resolved space."""
        name = self._lockfile_name(tool_id, version)

        if space == "project":
            return self.project_path / ".ai" / "lockfiles" / name
        # user and system both write to user space (system is read-only)
        return self.user_space / "lockfiles" / name
```

In `PrimitiveExecutor.execute()`, the space is available from `chain[0].space`:

```python
# Step 7: Create lockfile on success
new_lockfile = self.lockfile_resolver.create_lockfile(...)
self.lockfile_resolver.save_lockfile(new_lockfile, space=chain[0].space)
```

**Read precedence stays the same**: project → user → system. A project lockfile correctly shadows a stale user lockfile for the same tool.

**Note**: The project lockfile path should be `.ai/lockfiles/` (under `.ai/`), not `{project}/lockfiles/` at the project root. This keeps lockfiles inside the `.ai/` managed directory, consistent with where all other Rye artifacts live and where the bundler looks for them (`lockfiles/{bundle_slug}_*.lock.yaml`).

The current `LockfileResolver.project_dir` returns `project_path / "lockfiles"` — this should change to `project_path / ".ai" / "lockfiles"` to align with the bundler's expectations and the `.ai/` convention.

## Alignment with Bundle Architecture

### How Anchor verify_deps and Bundle Manifests Relate

The anchor system and the bundle system operate at different layers but converge on the same invariant: **all bytes that influence execution must be authenticated**.

| Concern            | Anchor `verify_deps`             | Bundle Manifest                 |
| ------------------ | -------------------------------- | ------------------------------- |
| **Granularity**    | Per-tool directory               | Per-bundle (multi-tool)         |
| **When created**   | Automatically on first execution | Explicitly via `bundler create` |
| **When verified**  | Pre-spawn (PrimitiveExecutor)    | On `bundler verify` or `pull`   |
| **Hash storage**   | Lockfile `verified_deps`         | `manifest.yaml` `files` dict    |
| **Signature type** | Inline `rye:signed:` per file    | Manifest-level Ed25519          |
| **Scope**          | Code files in tool directory     | All files in bundle tree        |

The key insight: **they don't compete — they compose**. When a tool is part of a bundle, both layers apply:

1. **Bundle pull** → manifest signature verified → per-file SHA256 checked → files written to `.ai/`
2. **Tool execution** → anchor `verify_deps` verifies inline signatures → lockfile caches the result
3. **Subsequent executions** → lockfile fast path skips both walks

### Bundle Pull Creates the Foundation for Anchor Verification

After `pull_bundle` writes files to `.ai/tools/{bundle_id}/`, every code file has an inline `rye:signed:` signature (placed there by the bundle author before `bundler create`). When the tool is later executed:

```
pull_bundle → writes .ai/tools/apps/task-manager/*.py (all inline-signed)
            → writes .ai/bundles/apps/task-manager/manifest.yaml

execute tool → anchor detects markers → verify_deps walks .ai/tools/apps/task-manager/
            → verify_item() checks each file's inline signature ✓
            → lockfile stores verified_deps hashes
            → next execution: lockfile fast path
```

### Identified Misalignment: Registry `push_bundle` Manifest Parsing

The bundler's `_create` action produces `manifest["files"]` as a **dict** keyed by relative path:

```yaml
files:
  .ai/tools/apps/task-manager/dev_server.py:
    sha256: 1a2b3c4d...
    inline_signed: true
```

But `push_bundle` in `registry.py` iterates `manifest["files"]` as a **list**:

```python
for file_entry in manifest["files"]:
    rel_path = file_entry if isinstance(file_entry, str) else file_entry.get("path", "")
```

When `manifest["files"]` is a dict, iterating it yields **keys** (the path strings), not the full entry objects. The `file_entry.get("sha256")` call would fail because `file_entry` is a string, not a dict. The `isinstance(file_entry, str)` guard catches this but loses the SHA256, meaning verification is silently skipped.

Same issue in `pull_bundle`'s verification loop — it expects a list of dicts with `path` keys.

**Fix**: Both registry functions should iterate the dict properly:

```python
# push_bundle — correct iteration for dict-shaped manifest["files"]
file_entries = manifest.get("files", {})
for rel_path, meta in file_entries.items():
    expected_sha = meta.get("sha256") if isinstance(meta, dict) else None
    # ...
```

### Lockfile Path Alignment with Bundler

The bundler's `_collect_bundle_files` looks for lockfiles at:

```python
lockfiles_dir = project_path / ".ai" / "lockfiles"
prefix = f"{bundle_slug}_"
```

With the lockfile scope change above (project lockfiles at `.ai/lockfiles/`), bundler and lockfile resolver agree on the path. Currently `LockfileResolver.project_dir` returns `project_path / "lockfiles"` (no `.ai/`), which means the bundler would never find project lockfiles to include in bundles.

### End-to-End Bundle Flow (After These Changes)

```
Author side:
  1. rye_sign → inline-signs all .py/.yaml/.md files
  2. bundler create → walks .ai/, hashes files, writes signed manifest
  3. Tool execution → anchor verify_deps + lockfile created at .ai/lockfiles/
  4. bundler create → includes .ai/lockfiles/ in manifest
  5. registry push_bundle → verifies manifest + files, uploads

Consumer side:
  1. registry pull_bundle → downloads manifest + files → writes to .ai/
  2. bundler verify → checks manifest sig + file hashes
  3. Tool execution → anchor detects markers → verify_deps → lockfile created
  4. Subsequent executions → lockfile fast path (hash comparison only)
```

## Future Extensions

- **Import hooks**: Python `meta_path` finder or Node `--experimental-loader` that blocks imports outside verified roots. Higher security but more invasive. The directory verification approach is "good enough" until tools become adversarial.
- **Per-tool anchor overrides**: A tool could declare `__anchor_root__ = "parent"` to override the runtime default. Not needed today — the runtime-level config covers all current cases.
