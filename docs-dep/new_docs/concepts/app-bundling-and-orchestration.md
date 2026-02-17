# App Bundling and LLM-Orchestrated Applications

How Rye OS bundles, signs, and executes entire applications as data-driven tooling — giving an LLM complete control over both the code and the runtime.

## The Core Idea

A traditional web application is a pile of source files that a human builds, deploys, and operates. In Rye OS, an application is a **bundle of directives, tools, and knowledge** that an LLM orchestrates end-to-end — from scaffolding the project, to writing features, to running the dev server and tests, to packaging for distribution. The application code runs through the same `node_runtime` → `subprocess` primitive chain that every other Rye OS tool uses.

This means:

- The LLM doesn't just write code — it controls the entire lifecycle through directives
- Every action is tracked in the thread transcript (audit trail for free)
- Every capability is declared and enforced via CapabilityToken — once [token propagation](thread-orchestration-internals.md#gap-4-capability-token-propagation) is implemented — Phase A4 in the [implementation plan](../IMPLEMENTATION_PLAN.md#a4-capability-token-propagation) (security for free)
- The whole thing packages into a shareable MCP bundle (distribution for free)
- Work happens through git — branches, commits, PRs — the normal way

The primitive runtime is not a limitation. It is the **control plane** that makes the application observable, sandboxed, and reproducible.

## Signing Architecture

### The Current Trust Boundary (and Its Gap)

`PrimitiveExecutor.execute()` calls `verify_item()` on every element in the resolution chain — but the chain only contains files with `__executor_id__` metadata. Files loaded via `importlib` inside a signed tool are **not in the chain** and **not verified**.

In the `.ai/tools/rye/agent/threads/` directory (Python module path: `rye.agent.threads`), this produces an exact split:

| Signed (has `__executor_id__`, in chain) | Unsigned (no `__executor_id__`, loaded via importlib) |
| ---------------------------------------- | ----------------------------------------------------- |
| `thread_directive.py`                    | `safety_harness.py`                                   |
| `spawn_thread.py`                        | `expression_evaluator.py`                             |
| `thread_registry.py`                     | `core_helpers.py`                                     |
| `read_transcript.py`                     | `conversation_mode.py`                                |
| `thread_telemetry.py`                    | `approval_flow.py`                                    |
| `transcript_renderer.py`                 | `thread_channels.py`                                  |
|                                          | `transcript_watcher.py`                               |

> **Path convention:** `.ai/tools/...` paths are project-runtime paths (under the user's project root). The corresponding repo path is `rye/rye/.ai/tools/...`. Design docs use project-runtime paths unless prefixed with `rye/rye/`.

The import chain looks like this:

```
thread_directive.py (SIGNED, verified by PrimitiveExecutor)
  └── importlib.exec_module(safety_harness.py)       ← NOT VERIFIED
        └── importlib.exec_module(expression_evaluator.py)  ← NOT VERIFIED
        └── importlib.exec_module(capability_tokens.py)     ← NOT VERIFIED
```

`safety_harness.py` — the module that enforces permissions and cost limits — runs unverified. If it were tampered with, the signed `thread_directive.py` would load and execute the tampered harness without complaint. The executor never knows because `importlib` bypasses the chain entirely.

This is not an intentional design boundary. It's an artifact of which files were given `__executor_id__` headers. The real trust boundary is wider: **all bytes that can influence privileged execution must be authenticated**.

### Two-Layer Signing Model

The correct architecture uses two complementary mechanisms:

#### Layer 1: Verified Loader for Dynamic Dependencies

The gap is not Python-specific. The tool system already supports `.py`, `.yaml`, `.yml`, `.json`, `.js`, `.sh`, `.toml` — all data-driven from the tool extractor (`EXTENSIONS` list in `rye/core/extractors/tool/tool_extractor.py`). The signature format system (`get_signature_format()`) is also data-driven, loaded from the extractor's `SIGNATURE_FORMAT` dict. Any file type the extractor declares gets a comment prefix for inline signatures (`#` for Python/Shell, `//` for JS/TS, `<!-- -->` for Markdown, etc.).

So the verified loader is not a Python module loader — it's a language-agnostic pre-execution verification gate. It verifies the inline signature of any file under `.ai/tools/` before it's loaded or executed, regardless of how it's consumed:

```python
def verify_dependency(path: Path, project_path: Path) -> str:
    """Verify a tool dependency before load/execution.

    Works for any file type that the signature format system supports.
    Calls verify_item() which uses get_signature_format() to find the
    correct comment prefix for the file's extension — data-driven
    from the extractor's SIGNATURE_FORMAT dict.

    Returns the verified content hash on success.
    Raises IntegrityError on any failure.
    """
    real_path = path.resolve(strict=True)

    # Enforce allowed roots
    allowed_roots = [
        (project_path / ".ai" / "tools").resolve(strict=True),
        (get_user_space() / "tools").resolve(strict=True),
        (get_system_space() / "tools").resolve(strict=True),
    ]
    if not any(is_subpath(real_path, root) for root in allowed_roots):
        raise IntegrityError(f"Dependency outside allowed roots: {real_path}")

    # Disallow symlink tricks that escape allowed roots
    if path.is_symlink():
        raise IntegrityError(f"Symlinked dependency not allowed: {path}")

    # verify_item handles all file types via MetadataManager → get_signature_format().
    # ItemType.TOOL is used because all files under .ai/tools/ are tool artifacts,
    # even helper modules without __executor_id__. The signing system only needs
    # the file extension to find the correct comment prefix — it does not require
    # tool metadata headers for signature verification.
    return verify_item(real_path, ItemType.TOOL, project_path=project_path)
```

The calling code still uses whatever loading mechanism is appropriate for the file type. `verify_dependency()` runs first, then the load proceeds:

| File Type | How It's Loaded | Where `verify_dependency()` Runs |
|-----------|----------------|----------------------------------|
| `.py` | `importlib.util.exec_module()` | Before `exec_module()` |
| `.js` | `node_runtime` → `subprocess` → `node file.js` | Before subprocess spawn |
| `.sh` | `subprocess` → `bash script.sh` | Before subprocess spawn |
| `.yaml` | `yaml.safe_load(path.read_text())` | Before `read_text()` |
| `.json` | `json.loads(path.read_text())` | Before `read_text()` |

No new format handling is needed — `verify_item()` already delegates to `MetadataManager` which delegates to `get_signature_format()` which loads from the extractor. Adding a new language is just adding a new extractor with its `EXTENSIONS` and `SIGNATURE_FORMAT` — the verification pipeline picks it up automatically.

The key enforcement points:

1. **`PrimitiveExecutor._execute_builtin()`** (line 736) — currently loads Python modules via raw `importlib` without verification
2. **Every `importlib.util.spec_from_file_location` call in tools** — e.g., `thread_directive.py` loading `safety_harness.py`
3. **Subprocess-based execution** — when a tool shells out to run a `.js` or `.sh` dependency

All three must call `verify_dependency()` before proceeding. For builtins, prefer a
`VerifiedModuleLoader` helper that wraps `importlib` so a dependency can't bypass
verification even if new loaders are added later.

After this change, the threads directory split becomes:

| File                      | Has `__executor_id__` | Signature Required | Why                                            |
| ------------------------- | --------------------- | ------------------ | ---------------------------------------------- |
| `thread_directive.py`     | Yes                   | Yes                | Chain element — verified by PrimitiveExecutor  |
| `safety_harness.py`       | No                    | **Yes**            | Loaded by signed tool — verified before load   |
| `expression_evaluator.py` | No                    | **Yes**            | Loaded by safety_harness — verified before load |
| `capability_tokens.py`    | No                    | **Yes**            | Loaded by safety_harness — verified before load |
| All other helpers         | No                    | **Yes**            | Loaded by signed tools — verified before load  |

The rule: **if it's under `.ai/tools/` and it gets loaded at runtime, it must be signed.** The file type doesn't matter — the signature format system handles that data-driven.

#### Layer 2: Bundle Manifest for App Assets

Per-file Ed25519 signatures work for code files (`.py`, `.js`, `.sh`, `.yaml`) because the signing system can embed a comment-line signature. But for app bundles — React components, CSS, images, WASM, node_modules — inline signatures don't work. You can't embed a `// rye:signed:...` comment in a PNG or a minified JS bundle.

The solution is a **signed bundle manifest**: a single file that lists every file in the bundle with its SHA256 hash. The manifest itself is signed with Ed25519.

The manifest lives at `.ai/bundles/{bundle_id}/manifest.yaml` — for this example, `.ai/bundles/apps/task-manager/manifest.yaml`.

```yaml
# rye:signed:2026-02-11T00:00:00Z:MANIFEST_HASH:ED25519_SIG:PUBKEY_FP
bundle:
  id: apps/task-manager
  version: 1.0.0
  created: 2026-02-11T00:00:00Z
  entrypoint:
    item_type: directive
    item_id: apps/task-manager/build_crud_app
  description: CRUD task manager with React + Express + SQLite

files:
  # Directives (also have inline signatures)
  directives/apps/task-manager/build_crud_app.md:
    sha256: a1b2c3d4...
    inline_signed: true
  directives/apps/task-manager/scaffold_project.md:
    sha256: e5f6a7b8...
    inline_signed: true
  directives/apps/task-manager/implement_feature.md:
    sha256: c9d0e1f2...
    inline_signed: true

  # Tools (also have inline signatures)
  tools/apps/task-manager/dev_server.py:
    sha256: 1a2b3c4d...
    inline_signed: true
  tools/apps/task-manager/test_runner.py:
    sha256: 5e6f7a8b...
    inline_signed: true
  tools/apps/task-manager/build.py:
    sha256: 9c0d1e2f...
    inline_signed: true

  # Knowledge (also have inline signatures)
  knowledge/apps/task-manager/react-patterns.md:
    sha256: 3a4b5c6d...
    inline_signed: true

  # Plans (manifest hash only — no inline signatures)
  plans/task-manager/phase_1/plan_db_schema.md:
    sha256: 7e8f9a0b...
  plans/task-manager/phase_1/plan_api_routes.md:
    sha256: 1c2d3e4f...

  # Lockfiles (manifest hash only)
  lockfiles/apps_task-manager_build_crud_app.lock.yaml:
    sha256: 5a6b7c8d...
```

At runtime, when any tool or directive loads a file from the bundle, verification checks either:

1. `verify_item(path)` — inline Ed25519 signature (for code files), **OR**
2. `verify_bundle_manifest(bundle_id)` + `sha256(file) == manifest[file]` — manifest coverage (for everything else)

Either path must succeed. Files that have inline signatures get both checks (belt and suspenders). Files without inline signatures (JSX, CSS, images, generated bundles) are covered by the manifest hash alone.

### Where the Line Is

Three categories of files, three enforcement mechanisms:

| Category            | Example Files                                        | Inline `rye:signed:` | Bundle Manifest    | When Verified                      |
| ------------------- | ---------------------------------------------------- | -------------------- | ------------------ | ---------------------------------- |
| **Chain elements**  | `thread_directive.py`, `node_runtime.py`             | Yes                  | Yes (if in bundle) | `PrimitiveExecutor._build_chain()` |
| **Dynamic imports** | `safety_harness.py`, `expression_evaluator.py`       | Yes                  | Yes (if in bundle) | `load_verified_module()`           |
| **App assets**      | `App.jsx`, `tasks.js`, `favicon.ico`, `package.json` | No                   | **Yes**            | Tool load time via manifest check  |

The rule: **every file that influences execution must be authenticated before use.** The mechanism differs by file type, but the invariant is the same.

### What This Means for App Bundles

For a bundled CRUD app like the task-manager example:

- **Directives** (5 files): inline Ed25519 signatures + manifest hashes
- **Tools** (4 `.py` files): inline Ed25519 signatures + manifest hashes
- **Knowledge** (3 `.md` files): inline Ed25519 signatures + manifest hashes
- **App source** (~15 `.js`/`.jsx`/`.css` files): manifest hashes only
- **Static assets** (images, fonts): manifest hashes only
- **Config** (`package.json`, `.gitignore`): manifest hashes only

Total signatures to manage: **12 inline** (directives + tools + knowledge) + **1 manifest** signature that covers everything including plans, lockfiles, and app source. The user signs 12 code files via `rye_sign` and the bundler tool (`action=create`) generates the manifest automatically.

### Manifest Generation and Verification

#### Generation (at bundle time)

```
1. Walk all files under the bundle's directory tree
2. For each file: compute sha256, record whether it has an inline signature
3. Write the manifest YAML with all file entries
4. Sign the manifest itself with Ed25519 (inline comment on line 1)
```

This is an action on the bundler core tool — `.ai/tools/rye/core/bundler/bundler.py` `action=create` — that runs through the standard execution chain. It uses `MetadataManager.compute_hash()` for consistency with the existing integrity system. See [bundler-tool-architecture.md](bundler-tool-architecture.md) for the full tool interface.

#### Verification (at load time)

```
1. Load manifest, verify its Ed25519 signature (same as any tool)
2. For each file the current operation needs:
   a. If file has inline signature: verify_item() (existing path)
   b. If file is in manifest: compute sha256, compare to manifest entry
   c. If neither: reject — file is not authenticated
3. Cache verification results keyed by (realpath, content_hash)
```

The manifest is verified once per execution. Individual file hashes are checked lazily — only when a file is actually used. This avoids scanning the entire bundle upfront for operations that only touch a few files.

### The Runtime Stack

```
app_build.py ──→ node_runtime.py ──→ subprocess primitive ──→ npm run build
app_serve.py ──→ node_runtime.py ──→ subprocess primitive ──→ node server.js
app_test.py  ──→ node_runtime.py ──→ subprocess primitive ──→ npm test
```

`node_runtime.py` is 42 lines of config. It resolves the Node interpreter via `ENV_CONFIG`, sets `NODE_ENV`, and delegates to `rye/core/primitives/subprocess`. The app code runs under the same SafetyHarness and CapabilityToken enforcement as any other tool.

### Capability Confinement

The orchestrator directive declares what the app is allowed to do:

```xml
<permissions>
  <execute>
    <tool>rye.file-system.*</tool>
    <tool>rye.core.runtimes.node_runtime</tool>
  </execute>
  <search>
    <knowledge>*</knowledge>
  </search>
</permissions>
```

Child threads spawned for individual features get **attenuated** tokens — the intersection of parent and child declared capabilities. A `write_component` child thread cannot access the database migration tool if it didn't declare that permission.

### Lockfile Pins the Tool Chain

When the orchestrator executes successfully, the lockfile resolver records SHA256 integrity hashes for every element in every resolution chain. On subsequent execution:

1. Lockfile integrity check runs first (any content change → blocked)
2. Ed25519 signature verification runs for every chain element
3. Both must pass

The lockfile covers the **tool chain** (the `__executor_id__` resolution path). The **bundle manifest** covers everything else (app source, assets, config). Together they pin the entire bundle. If both are present, use the lockfile for tool chain integrity and the manifest for non-chain assets; neither replaces the other.

## Worked Example: CRUD Task Manager

A React + Express + SQLite task manager, orchestrated end-to-end by an LLM, packaged as a shareable MCP bundle.

### What the User Does

```
run directive build_crud_app with inputs: {
  "name": "task-manager",
  "description": "CRUD task manager with React frontend, Express API, SQLite database",
  "features": "create tasks, mark complete, delete, filter by status"
}
```

The orchestrator takes over from here.

### Bundle Structure

```
.ai/
├── bundles/
│   └── apps/task-manager/
│       └── manifest.yaml              ← signed bundle manifest (bundler tool creates this)
│
├── directives/
│   └── apps/task-manager/
│       ├── build_crud_app.md          ← signed orchestrator (entry point)
│       ├── scaffold_project.md        ← signed, scaffolds directories + package.json
│       ├── implement_feature.md       ← signed, writes one feature end-to-end
│       ├── run_tests.md               ← signed, executes test suite
│       └── build_and_serve.md         ← signed, production build + dev server
│
├── tools/
│   └── apps/task-manager/
│       ├── dev_server.py              ← node_runtime: npm run dev
│       ├── test_runner.py             ← node_runtime: npm test
│       ├── build.py                   ← node_runtime: npm run build
│       └── db_migrate.py             ← node_runtime: npx prisma migrate
│
├── knowledge/
│   └── apps/task-manager/
│       ├── react-patterns.md          ← component conventions for this app
│       ├── api-design.md              ← REST endpoint patterns
│       └── db-schema.md               ← data model reference
│
├── plans/
│   └── task-manager/
│       └── phase_1/
│           ├── plan_db_schema.md
│           ├── plan_api_routes.md
│           ├── plan_react_ui.md
│           └── plan_integration.md
│
├── threads/                           ← runtime state (transcript, registry)
│   ├── registry.db
│   └── build_crud_app-1739012630/
│       ├── thread.json
│       └── transcript.jsonl
│
└── lockfiles/
    └── apps_task-manager_build_crud_app.lock.yaml
```

The app source code lives in the normal project directory alongside the `.ai/` bundle:

```
task-manager/
├── server/
│   ├── index.js
│   ├── routes/
│   │   └── tasks.js
│   └── db/
│       ├── schema.sql
│       └── seed.js
├── client/
│   ├── src/
│   │   ├── App.jsx
│   │   ├── components/
│   │   │   ├── TaskList.jsx
│   │   │   ├── TaskForm.jsx
│   │   │   └── TaskFilter.jsx
│   │   └── api.js
│   └── package.json
├── package.json
└── .gitignore
```

### The Orchestrator Directive

This is the top-level entry point. It plans, coordinates waves of parallel work, and runs verification.

````xml
<!-- rye:signed:2026-02-11T00:00:00Z:CONTENT_HASH:ED25519_SIG:PUBKEY_FP -->

# Build CRUD App

```xml
<directive name="build_crud_app" version="1.0.0">
  <metadata>
    <description>Orchestrate full-stack CRUD app creation — plan, scaffold, implement features in waves, test, build, and package as MCP bundle.</description>
    <category>apps/task-manager</category>
    <author>rye</author>
    <model tier="sonnet" id="claude-sonnet-4-20250514" />
    <limits max_turns="30" max_tokens="80000" max_spawns="8" max_spend="3.00" />
    <permissions>
      <execute>
        <tool>rye.file-system.*</tool>
        <tool>rye.core.runtimes.node_runtime</tool>
        <directive>apps.task-manager.*</directive>
      </execute>
      <search>
        <knowledge>*</knowledge>
        <directive>*</directive>
      </search>
      <load>
        <knowledge>*</knowledge>
      </load>
    </permissions>
  </metadata>

  <inputs>
    <input name="name" type="string" required="true">
      Application name in kebab-case (e.g., "task-manager")
    </input>
    <input name="description" type="string" required="true">
      What the app does — used to generate the implementation plan
    </input>
    <input name="features" type="string" required="true">
      Comma-separated feature list (e.g., "create tasks, mark complete, delete, filter by status")
    </input>
  </inputs>

  <process>
    <step name="load_patterns">
      <description>Load knowledge entries for React component patterns, API design conventions, and database schema patterns to inform the plan.</description>
      <load item_type="knowledge" item_id="apps/{input:name}/react-patterns" />
      <load item_type="knowledge" item_id="apps/{input:name}/api-design" />
      <load item_type="knowledge" item_id="apps/{input:name}/db-schema" />
    </step>

    <step name="plan">
      <description>
        Break the app description and feature list into a phased implementation plan.
        Group work into waves based on dependencies:

        Wave 1 (parallel, no dependencies):
          - plan_db_schema: SQLite schema, migrations, seed data
          - plan_api_routes: Express route handlers for CRUD operations

        Wave 2 (parallel, depends on Wave 1):
          - plan_react_ui: React components, pages, API client

        Wave 3 (sequential, depends on all above):
          - plan_integration: Wire frontend to backend, end-to-end test

        Write each plan to .ai/plans/{input:name}/phase_1/plan_{name}.md with frontmatter:
        wave number, dependencies, must_have observable truths.
      </description>
      <execute item_type="tool" item_id="rye/file-system/fs_write">
        <param name="path" value=".ai/plans/{input:name}/phase_1/" />
        <param name="content" value="Generated plan documents" />
      </execute>
    </step>

    <step name="scaffold">
      <description>Create project directory structure, package.json files, and base configuration by executing the scaffold directive.</description>
      <execute item_type="directive" item_id="apps/{input:name}/scaffold_project">
        <param name="name" value="{input:name}" />
        <param name="description" value="{input:description}" />
      </execute>
    </step>

    <step name="wave_1" parallel="true">
      <description>
        Execute Wave 1 plans in parallel — these have no dependencies on each other.
        Spawn a child thread for each plan. Each child runs the implement_feature directive
        with attenuated capabilities (only file-system write + node_runtime).
      </description>
      <execute item_type="directive" item_id="apps/{input:name}/implement_feature">
        <param name="plan_path" value=".ai/plans/{input:name}/phase_1/plan_db_schema.md" />
        <param name="name" value="{input:name}" />
      </execute>
      <execute item_type="directive" item_id="apps/{input:name}/implement_feature">
        <param name="plan_path" value=".ai/plans/{input:name}/phase_1/plan_api_routes.md" />
        <param name="name" value="{input:name}" />
      </execute>
    </step>
    <!-- parallel="true" is advisory to the LLM. Actual parallelism requires the LLM 
         to emit spawn_thread(execute_directive=true) calls + wait_threads. See 
         thread-orchestration-internals.md § "How the Orchestrator Uses It" for the 
         exact tool-call sequence. -->

    <step name="wave_2" parallel="true">
      <description>
        Execute Wave 2 plans — depend on Wave 1 completion.
        API routes and DB schema are done, so React components can now call real endpoints.
      </description>
      <execute item_type="directive" item_id="apps/{input:name}/implement_feature">
        <param name="plan_path" value=".ai/plans/{input:name}/phase_1/plan_react_ui.md" />
        <param name="name" value="{input:name}" />
      </execute>
    </step>

    <step name="wave_3">
      <description>
        Execute Wave 3 — integration. Wire frontend to backend, run end-to-end checks.
        This is sequential since it touches everything.
      </description>
      <execute item_type="directive" item_id="apps/{input:name}/implement_feature">
        <param name="plan_path" value=".ai/plans/{input:name}/phase_1/plan_integration.md" />
        <param name="name" value="{input:name}" />
      </execute>
    </step>

    <step name="test">
      <description>Run the full test suite via the test_runner tool. If tests fail, analyze errors and spawn a fix directive.</description>
      <execute item_type="tool" item_id="apps/{input:name}/test_runner" />
    </step>

    <step name="build">
      <description>Run the production build via the build tool. Verify output exists in dist/.</description>
      <execute item_type="tool" item_id="apps/{input:name}/build" />
    </step>
  </process>

  <hooks>
    <hook event="limit" when="turns_exceeded">
      <directive>apps/{input:name}/build_and_serve</directive>
      <inputs>
        <input name="action" value="status_report" />
      </inputs>
    </hook>
    <hook event="after_complete">
      <directive>apps/{input:name}/build_and_serve</directive>
      <inputs>
        <input name="action" value="start_dev" />
        <input name="name" value="{input:name}" />
      </inputs>
    </hook>
  </hooks>

  <success_criteria>
    <criterion>All plan must_haves verified as true</criterion>
    <criterion>Test suite passes with zero failures</criterion>
    <criterion>Production build completes without errors</criterion>
    <criterion>Dev server starts and responds on localhost</criterion>
  </success_criteria>

  <outputs>
    <success>
      App '{input:name}' built and running. Thread transcript at .ai/threads/.
      Bundle shareable via: rye_execute item_type=tool item_id=rye/core/bundler/bundler action=create bundle_id=apps/{input:name}
      Then: rye_execute item_type=tool item_id=rye/core/registry/registry action=push_bundle bundle_id=apps/{input:name}
    </success>
    <failure>
      Build failed. Check thread transcript for error details.
      Resume with: resume_thread thread_id={thread_id}
    </failure>
  </outputs>
</directive>
````

### The Scaffold Directive

Creates the initial project structure — directories, package.json, base config files.

````xml
<!-- rye:signed:2026-02-11T00:00:00Z:CONTENT_HASH:ED25519_SIG:PUBKEY_FP -->

# Scaffold Project

```xml
<directive name="scaffold_project" version="1.0.0">
  <metadata>
    <description>Create project directory structure, package.json files, and base configuration for a full-stack React + Express + SQLite application.</description>
    <category>apps/task-manager</category>
    <author>rye</author>
    <model tier="haiku" id="claude-3-5-haiku-20241022" />
    <limits max_turns="10" max_tokens="8192" max_spend="0.50" />
    <permissions>
      <execute>
        <tool>rye.file-system.*</tool>
        <tool>rye.core.runtimes.node_runtime</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="name" type="string" required="true">
      Application name in kebab-case
    </input>
    <input name="description" type="string" required="true">
      Application description for package.json
    </input>
  </inputs>

  <process>
    <step name="create_structure">
      <description>
        Create the project directory tree:
        {input:name}/
        ├── server/
        │   ├── index.js         (Express entry point with CORS, JSON body parser)
        │   ├── routes/           (empty, features will add route files)
        │   └── db/
        │       └── schema.sql    (empty, db_schema plan will populate)
        ├── client/
        │   ├── src/
        │   │   ├── App.jsx       (root component with router shell)
        │   │   ├── components/   (empty, react_ui plan will populate)
        │   │   ├── pages/        (empty)
        │   │   └── api.js        (base API client with fetch wrapper)
        │   ├── public/
        │   │   └── index.html
        │   └── package.json      (react, react-dom, react-router-dom, vite)
        ├── package.json          (root: express, better-sqlite3, concurrently)
        └── .gitignore
      </description>
      <execute item_type="tool" item_id="rye/file-system/fs_write">
        <param name="path" value="{input:name}/" />
        <param name="content" value="Generated project structure" />
        <param name="create_dirs" value="true" />
      </execute>
    </step>

    <step name="install_deps">
      <description>Run npm install in the project root and client directory to set up node_modules.</description>
      <execute item_type="tool" item_id="apps/{input:name}/dev_server">
        <param name="action" value="install" />
      </execute>
    </step>

    <step name="verify_scaffold">
      <description>
        Verify the scaffold is correct:
        - package.json exists at root and client/
        - node_modules/ exists at root and client/
        - server/index.js is syntactically valid
        - client/src/App.jsx is syntactically valid
      </description>
    </step>
  </process>

  <outputs>
    <success>Project scaffolded at {input:name}/. Ready for feature implementation.</success>
    <failure>Scaffold failed — check that Node.js is available and npm install succeeded.</failure>
  </outputs>
</directive>
````

### The Feature Implementation Directive

This is the workhorse — each child thread runs one of these to implement a single plan (db schema, api routes, react ui, or integration).

````xml
<!-- rye:signed:2026-02-11T00:00:00Z:CONTENT_HASH:ED25519_SIG:PUBKEY_FP -->

# Implement Feature

```xml
<directive name="implement_feature" version="1.0.0">
  <metadata>
    <description>Execute a single implementation plan — read the plan document, implement each task, verify must_haves, and handle deviations. This directive is designed to be spawned as a child thread by the orchestrator.</description>
    <category>apps/task-manager</category>
    <author>rye</author>
    <model tier="sonnet" id="claude-sonnet-4-20250514" />
    <limits max_turns="15" max_tokens="30000" max_spend="1.00" />
    <permissions>
      <execute>
        <tool>rye.file-system.*</tool>
        <tool>rye.core.runtimes.node_runtime</tool>
      </execute>
      <load>
        <knowledge>*</knowledge>
      </load>
    </permissions>
  </metadata>

  <inputs>
    <input name="plan_path" type="string" required="true">
      Path to the plan document (e.g., .ai/plans/task-manager/phase_1/plan_db_schema.md)
    </input>
    <input name="name" type="string" required="true">
      Application name — used to resolve file paths within the app directory
    </input>
  </inputs>

  <process>
    <step name="load_plan">
      <description>Read the plan document and parse its frontmatter (wave, depends_on, must_haves) and task list (files, action, verify, done for each task).</description>
      <execute item_type="tool" item_id="rye/file-system/fs_read">
        <param name="path" value="{input:plan_path}" />
      </execute>
    </step>

    <step name="execute_tasks">
      <description>
        For each task in the plan, in order:

        1. Read the task details: target files, action description, verification command, done criteria
        2. Write the code — create or modify files as specified
        3. Run verification if a command is provided (e.g., node -c file.js for syntax check)
        4. If verification passes: mark task done, continue to next
        5. If verification fails: analyze the error, fix the code, retry (max retries configurable via plan frontmatter)

        Deviation handling:
        - Bug or missing-critical: auto-fix without asking (record in transcript)
        - Architectural change: halt and report back to orchestrator via thread status
      </description>
      <execute item_type="tool" item_id="rye/file-system/fs_write">
        <param name="path" value="Generated file paths from plan" />
        <param name="content" value="Generated code from plan tasks" />
      </execute>
    </step>

    <step name="verify_must_haves">
      <description>
        After all tasks complete, verify each must_have from the plan frontmatter.
        Each must_have is an observable truth (e.g., "tasks table exists in schema.sql",
        "GET /api/tasks returns 200"). Check by reading files or running commands.
        Record pass/fail for each.
      </description>
    </step>

    <step name="commit_work">
      <description>
        Stage and commit all files changed by this plan.
        Commit message: "feat({input:name}): {plan_name} — {summary}"
        This gives the orchestrator a clean git boundary per plan.
      </description>
    </step>
  </process>

  <outputs>
    <success>Plan complete. All must_haves verified. Committed as: {commit_hash}</success>
    <failure>Plan incomplete — {failed_must_haves} must_haves failed. See transcript for details.</failure>
  </outputs>
</directive>
````

### The Bundle Tools

These are thin wrappers around `node_runtime` that give the LLM named tools for each app lifecycle operation.

#### dev_server.py

```python
# rye:signed:2026-02-11T00:00:00Z:CONTENT_HASH:ED25519_SIG:PUBKEY_FP
"""Dev Server Tool — start, stop, or install deps for the application."""

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/node_runtime"
__category__ = "apps/task-manager"
__tool_description__ = "Manage the development server — install deps, start, stop"

ENV_CONFIG = {
    "interpreter": {
        "type": "node_modules",
        "search_paths": ["node_modules/.bin"],
        "var": "RYE_NODE",
    },
    "env": {
        "NODE_ENV": "development",
    },
}

CONFIG = {
    "command": "${RYE_NODE}",
    "args": [],
    "timeout": 300,
}

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "action": {
            "type": "string",
            "enum": ["install", "start", "stop", "status"],
            "description": "Server action to perform",
        },
        "port": {
            "type": "integer",
            "description": "Port to serve on",
            "default": 3000,
        },
    },
    "required": ["action"],
}
```

#### test_runner.py

```python
# rye:signed:2026-02-11T00:00:00Z:CONTENT_HASH:ED25519_SIG:PUBKEY_FP
"""Test Runner Tool — execute the application test suite."""

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/node_runtime"
__category__ = "apps/task-manager"
__tool_description__ = "Run the test suite via npm test and report results"

ENV_CONFIG = {
    "interpreter": {
        "type": "node_modules",
        "search_paths": ["node_modules/.bin"],
        "var": "RYE_NODE",
    },
    "env": {
        "NODE_ENV": "test",
        "CI": "true",
    },
}

CONFIG = {
    "command": "${RYE_NODE}",
    "args": ["--test"],
    "timeout": 120,
}

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "suite": {
            "type": "string",
            "description": "Test suite to run (all, server, client)",
            "default": "all",
        },
        "watch": {
            "type": "boolean",
            "description": "Run in watch mode",
            "default": false,
        },
    },
}
```

#### build.py

```python
# rye:signed:2026-02-11T00:00:00Z:CONTENT_HASH:ED25519_SIG:PUBKEY_FP
"""Build Tool — production build for the application."""

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/node_runtime"
__category__ = "apps/task-manager"
__tool_description__ = "Run production build via npm run build"

ENV_CONFIG = {
    "interpreter": {
        "type": "node_modules",
        "search_paths": ["node_modules/.bin"],
        "var": "RYE_NODE",
    },
    "env": {
        "NODE_ENV": "production",
    },
}

CONFIG = {
    "command": "${RYE_NODE}",
    "args": [],
    "timeout": 300,
}

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "target": {
            "type": "string",
            "enum": ["client", "server", "all"],
            "description": "What to build",
            "default": "all",
        },
    },
}
```

## How the Orchestration Runs

### Thread Hierarchy

When the user runs `build_crud_app`, the thread system creates this hierarchy:

```
build_crud_app-1739012630 (orchestrator, sonnet, max_spend=$3.00)
├── scaffold_project-1739012631 (child, haiku, attenuated token)
├── implement_feature-1739012700 (child, sonnet, plan_db_schema)     ┐
├── implement_feature-1739012701 (child, sonnet, plan_api_routes)    ┤ Wave 1 (parallel)
├── implement_feature-1739012800 (child, sonnet, plan_react_ui)      ┘ Wave 2
├── implement_feature-1739012900 (child, sonnet, plan_integration)     Wave 3
└── build_and_serve-1739013000 (hook, haiku, after_complete)
```

Each child thread:

- Gets a CapabilityToken that is the **intersection** of the parent's capabilities and the child directive's declared permissions
- Runs under its own SafetyHarness with its own limits
- Writes to its own transcript file at `.ai/threads/{thread_id}/transcript.jsonl`
- Commits its work to a git branch, giving the orchestrator clean boundaries

> Thread statuses, tool names, and hook events follow the [Canonical Vocabulary](thread-orchestration-internals.md#canonical-vocabulary) defined in the orchestration internals doc.

### Wave Execution

The orchestrator follows the wave pattern from the plan:

```
Wave 1: [plan_db_schema, plan_api_routes]
         No dependencies. Spawned in parallel.
         Both can write to different directories simultaneously.

         db_schema writes:  server/db/schema.sql, server/db/seed.js
         api_routes writes: server/routes/tasks.js, server/index.js (add routes)

Wave 2: [plan_react_ui]
         Depends on api_routes (needs to know endpoint shapes).
         Awaits Wave 1 completion events before spawning.

         react_ui writes: client/src/components/TaskList.jsx, TaskForm.jsx, etc.

Wave 3: [plan_integration]
         Depends on all above.
         Wires frontend fetch calls to real API endpoints.
         Runs end-to-end verification.
```

Plans within the same wave run as parallel child threads. The orchestrator calls `wait_threads` to await completion events from all children in a wave — zero polling, instant failure detection. If any child fails, `fail_fast` mode cancels siblings and returns immediately so the orchestrator can re-plan.

### Deviation Handling

When a child thread encounters an unexpected situation:

| Category             | Action                         | Example                                  |
| -------------------- | ------------------------------ | ---------------------------------------- |
| **Bug**              | Auto-fix, record in transcript | Syntax error in generated JSX            |
| **Missing critical** | Auto-fix, record in transcript | Forgot to export component               |
| **Blocking**         | Auto-fix, record in transcript | Missing npm dependency                   |
| **Architectural**    | Halt, signal completion event → orchestrator re-plans | Need to add WebSocket instead of polling |
| **Scope expansion**  | Halt, report to orchestrator   | User wants real-time updates             |

Auto-fixes are recorded as transcript events so the orchestrator (and the user) can see exactly what happened. Architectural deviations suspend the child thread and bubble up to the orchestrator, which can either handle them or pause for human input.

### Git Integration

Each `implement_feature` child thread commits its work on completion:

```
feat(task-manager): db_schema — SQLite schema with tasks table and seed data
feat(task-manager): api_routes — Express CRUD routes for /api/tasks
feat(task-manager): react_ui — TaskList, TaskForm, TaskFilter components
feat(task-manager): integration — Wire frontend to API, add error handling
```

The orchestrator can inspect git diffs between waves to verify that Wave 1 output matches what Wave 2 expects. If there's a mismatch, the orchestrator re-plans Wave 2 with updated context.

## Sharing as an MCP Bundle

Once the app is built and working, the entire `.ai/` directory for the app is the bundle. To share it:

### What Gets Bundled

```
.ai/bundles/apps/task-manager/        ← signed bundle manifest (manifest.yaml)
.ai/directives/apps/task-manager/     ← signed directives (orchestrator + children)
.ai/tools/apps/task-manager/          ← signed tools (dev_server, test_runner, build)
.ai/knowledge/apps/task-manager/      ← knowledge entries (patterns, conventions)
.ai/plans/task-manager/               ← implementation plans (reproducible)
.ai/lockfiles/apps_task-manager_*.yaml ← pinned integrity hashes
```

### What Does NOT Get Bundled

```
.ai/threads/                          ← runtime state (transcripts, registry.db)
task-manager/node_modules/            ← npm dependencies (reproducible via package.json)
**/__pycache__/                       ← build artifacts
```

### How It's Shared

First, sign all individual items and create the bundle manifest using the bundler core tool:

```
# 1. Sign individual items (inline Ed25519 signatures)
rye_sign item_type=directive item_id=apps/task-manager/*
rye_sign item_type=tool item_id=apps/task-manager/*
rye_sign item_type=knowledge item_id=apps/task-manager/*

# 2. Create bundle manifest (walks .ai/, computes SHA256s, signs manifest)
rye_execute item_type=tool item_id=rye/core/bundler/bundler
  action=create
  bundle_id=apps/task-manager
  version=1.0.0
  entrypoint=apps/task-manager/build_crud_app
```

Then publish the bundle to the registry:

```
# 3. Push bundle (uploads manifest + all referenced files)
rye_execute item_type=tool item_id=rye/core/registry/registry
  action=push_bundle
  bundle_id=apps/task-manager
```

The registry re-signs the manifest with its own Ed25519 key (registry attestation) and appends provenance:

```
rye:signed:T:H:S:FP|registry@username
```

A recipient installs the bundle:

```
# 4. Pull bundle (downloads, verifies, extracts to .ai/)
rye_execute item_type=tool item_id=rye/core/registry/registry
  action=pull_bundle
  bundle_id=leolilley/apps/task-manager
  version=1.0.0
```

On pull, the manifest signature is verified (including registry provenance), content hashes are checked, and the bundle is written to the recipient's project space preserving directory structure. They can then run the entrypoint directive and the orchestrator will recreate the entire application from the plans and directives:

```
# 5. Run the app
rye_execute item_type=directive item_id=apps/task-manager/build_crud_app
  inputs='{"name": "task-manager", "description": "...", "features": "..."}'
```

### Reproducibility

Because the bundle contains:

- The orchestrator directive (what to build)
- The child directives (how to build each piece)
- The plans (exact task breakdown)
- The knowledge (conventions and patterns)
- The tools (runtime operations)
- The lockfile (pinned integrity hashes)

...the same bundle, run on a different machine, produces the same application. The LLM may generate slightly different code (it's nondeterministic), but the structure, the phases, the verification criteria, and the runtime tooling are all deterministic.

## What Makes This High-Leverage

### The LLM Controls Everything

In a traditional setup, the LLM writes code and the human handles everything else — project setup, dependency management, build configuration, testing, deployment. Here, the LLM:

1. **Plans** the work (wave-based task breakdown)
2. **Scaffolds** the project (directory structure, config files)
3. **Implements** features (code generation per plan)
4. **Tests** the result (runs test suite through node_runtime)
5. **Builds** for production (npm run build through node_runtime)
6. **Serves** the app (dev server through node_runtime)
7. **Handles errors** (deviation handling with auto-fix)
8. **Commits** the work (git integration per plan)

The human's role is reduced to: describe what you want, approve architectural decisions, and verify the result.

### The Runtime Is the Control Plane

Because `node_runtime` delegates to `subprocess`, every Node.js operation goes through the same enforcement as every other Rye OS tool:

- **CapabilityToken** — the app can only run npm commands that its directive declared
- **SafetyHarness** — the app can't exceed its token/spend/turn limits
- **Transcript** — every npm install, every test run, every build is logged with timing
- **Lockfile** — the exact tool chain is pinned and verified on every execution

This is the key difference from "LLM writes code, human runs it." The runtime itself is under the LLM's control but constrained by the directive's declarations. The LLM can iterate (run tests → fix code → run tests) without human intervention, but it can't exceed its declared permissions or resource limits.

### Apps Become Data

An application is no longer "a git repo with source code." It's a **bundle of directives, tools, knowledge, and plans** that produces source code as a side effect. The bundle is:

- **Shareable** — push to registry, pull on another machine
- **Reproducible** — same directives + plans = same structure
- **Inspectable** — read the directives to understand what it builds
- **Modifiable** — change a directive, re-run, get a different app
- **Composable** — mix directives from different bundles

The app's "source of truth" shifts from the generated code to the directives that generate it. The code is a build artifact. The directives are the specification.

### Conversation Continuity

Because the thread system supports multi-turn conversation (Phase 2 of agent threads), the user can:

```python
# Initial build
result = await execute(directive_name="build_crud_app", inputs={...})

# Add a feature later
result = await resume_thread(
    thread_id="build_crud_app-1739012630",
    message="Add user authentication with JWT tokens",
)

# Fix an issue
result = await resume_thread(
    thread_id="build_crud_app-1739012630",
    message="The TaskFilter component doesn't clear when switching between status tabs",
)
```

The orchestrator resumes from its saved harness state, loads the full conversation history from `transcript.jsonl`, and continues with full context. Cost tracking persists across turns — the cumulative spend is never lost.

> **opencode reference:** opencode achieves session continuity through a message-based storage model (session → messages → parts, all as JSON files) with an outer loop that handles compaction when token counts approach the context limit. The key insight: long-running sessions need compaction — summarizing old context to free up the context window. Rye OS's transcript-based approach (append-only JSONL) provides a better audit trail. Compaction is achieved through the hook-based policy layer: the harness emits a `context_window_pressure` event when token usage approaches the limit, and a user-defined hook directive (e.g., `compaction_summarizer`) performs the actual summarization. The tool-use loop applies the compaction result by reseeding the message list — `rebuild_conversation_from_transcript()` stays a faithful reconstructor, not a policy decision point. See [Compaction and Pruning](thread-orchestration-internals.md#compaction-and-pruning) for the full design.

## Comparison: Traditional vs. Rye OS App Development

| Aspect                 | Traditional                     | Rye OS Bundle                                            |
| ---------------------- | ------------------------------- | -------------------------------------------------------- |
| **Who plans**          | Human writes tickets/specs      | LLM generates wave-based plans from description          |
| **Who scaffolds**      | Human runs `create-react-app`   | Scaffold directive creates structure                     |
| **Who implements**     | Human writes code (LLM assists) | LLM writes code per plan, commits per feature            |
| **Who tests**          | Human runs `npm test`           | Test runner tool, triggered by directive                 |
| **Who handles errors** | Human reads errors, fixes       | Deviation handling: auto-fix bugs, escalate architecture |
| **Who builds**         | CI/CD pipeline                  | Build tool through node_runtime                          |
| **Runtime control**    | None — Node runs unconfined     | CapabilityToken + SafetyHarness + transcript             |
| **Sharing**            | Git clone + README              | Registry push/pull with signed lockfiles                 |
| **Resumability**       | Open editor, remember context   | `resume_thread` with full transcript history             |
| **Audit trail**        | Git log (coarse)                | Thread transcript (every LLM turn, tool call, result)    |
