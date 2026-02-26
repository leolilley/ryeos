<!-- rye:signed:2026-02-26T05:02:40Z:f773a958ec5ede98df99996b5d547015e6293fab3046a73a4aec3fb3e285b5b7:_4nN9Du9mB6H3sLvEUSBaKSPJ_uOHjo3f8w-gSVJdl98YZCbsR4ePxI6i9F3lXFnSUU5Ay31NNFm5pNYHBqKBA==:4b987fd4e40303ac -->
# Advanced Tools

Multi-file tools, the anchor system, bundles, and dependency management.

```xml
<directive name="advanced_tools" version="1.0.0">
  <metadata>
    <description>Guide 5: Multi-file tools, the anchor system, bundles, and dependency management.</description>
    <category>rye/guides</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="12" tokens="8192" />
    <permissions>
      <execute>
        <tool>rye.core.bundler.*</tool>
        <tool>rye.file-system.*</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs />

  <outputs>
    <output name="understanding">Understanding of multi-file tools, anchors, and bundles</output>
  </outputs>
</directive>
```

<process>
  <step name="intro">
    <render>
So far you've built single-file tools. But real tools often need helpers, shared utilities, configuration files. Rye handles this with multi-file tools and the anchor system. This is also where bundles come in — packaging tools for distribution.
</render>
    <instruction>Output ONLY the text inside the render block above. No preamble, no commentary.</instruction>
  </step>

  <step name="multi_file_tools">
    <render>
A multi-file tool is a directory with an entry point and helper modules. Look at the MCP tools for a real example:

```
.ai/tools/rye/mcp/
├── __init__.py      ← package marker
├── connect.py       ← entry point tool
├── discover.py      ← entry point tool
└── manager.py       ← entry point tool
```

Each `.py` file with `__executor_id__` is a separate tool. They share the directory and can import from each other. The executor uses `load_module()` to load tools, resolving imports relative to the tool's directory.

Another pattern — a tool with private helpers:

```
.ai/tools/my-project/scraper/
├── scraper.py       ← entry point (has __executor_id__)
├── parser.py        ← helper (no __executor_id__)
└── config.yaml      ← data file
```

Only `scraper.py` is callable as a tool. The helpers are just Python modules it imports.
</render>
    <instruction>Output the render block above, then have the user search for the MCP tools to see this in practice:</instruction>
    <tool_call>rye_search(scope="tool.rye.mcp.*", query="*")</tool_call>
  </step>

  <step name="anchor_system">
    <render>
Every tool has an anchor — the directory containing the tool file. This is how tools find their siblings.

```python
from pathlib import Path
_ANCHOR = Path(__file__).parent

# Load a config file next to the tool
config = yaml.safe_load((_ANCHOR / 'config.yaml').read_text())

# Import a helper module
from . import parser  # works because the directory is a package
```

The `_ANCHOR` pattern is used throughout Rye's own tools. The executor sets up the Python path so that relative imports work. When dependency verification is enabled, the executor also verifies all files in the anchor directory before execution — no tampered helpers.
</render>
    <instruction>Output ONLY the text inside the render block above.</instruction>
  </step>

  <step name="bundles">
    <render>
Bundles package multi-file tools (and directives and knowledge) for distribution. The manifest lists every file with its SHA256 hash.
</render>
    <instruction>Output the render block above, then list existing bundles:</instruction>
    <tool_call>rye_execute(item_type="tool", item_id="rye/core/bundler/bundler", parameters={"action": "list"})</tool_call>
    <render>
To create a bundle from your project's .ai/ directory:
</render>
    <tool_call>rye_execute(item_type="tool", item_id="rye/core/bundler/bundler", parameters={"action": "create", "bundle_id": "my-bundle", "version": "1.0.0"})</tool_call>
    <render>
To verify an existing bundle:
</render>
    <tool_call>rye_execute(item_type="tool", item_id="rye/core/bundler/bundler", parameters={"action": "verify", "bundle_id": "rye-os"})</tool_call>
    <instruction>Execute the verify command on the rye-os bundle above. Show the verification report to the user.</instruction>
    <render>
Dual protection — the manifest itself is signed, AND each file inside has its own signature. If anyone tampers with a single file, both the inline signature and the manifest hash catch it.
</render>
  </step>

  <step name="dependencies">
    <render>
Tools can declare pip dependencies that get installed on-demand:

```python
DEPENDENCIES = ["requests", "beautifulsoup4"]
```

The `EnvManager` creates an isolated venv for the tool and installs dependencies before execution. No manual pip install, no global pollution. Each tool gets exactly what it needs.
</render>
    <instruction>Output ONLY the text inside the render block above.</instruction>
  </step>

  <step name="next">
    <render>
Multi-file tools, anchors, bundles, dependencies. Tools can be as complex as they need to be — Rye handles the packaging, verification, and dependency management.

Next — connecting to external MCP servers:

```
rye execute directive mcp_discovery
```
</render>
    <instruction>Output ONLY the text inside the render block above.</instruction>
  </step>
</process>

<success_criteria>
<criterion>User understands multi-file tool structure and entry point vs helper distinction</criterion>
<criterion>User understands the anchor system and _ANCHOR pattern</criterion>
<criterion>User has seen bundle list, creation syntax, and a live verification report</criterion>
<criterion>User understands on-demand dependency management via DEPENDENCIES and EnvManager</criterion>
</success_criteria>
