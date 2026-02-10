# Rye OS Architecture & Implementation Context

This document provides complete architectural context for Rye OS, including code excerpts and design patterns.

---

## Overview

Rye OS is an MCP (Model Context Protocol) server that enables portability of AI agent workflows, tools, and knowledge across different AI coding environments (Claude Desktop, Cursor, Windsurf, etc.).

**Core Principle**: Everything is data-driven and filesystem-based. No hardcoded executor IDs or workflow logic—all resolved from `.ai/` directory metadata.

---

## The 4 MCP Tools (Public Interface)

### Tool Definitions (from `rye/server.py`)

```python
@self.server.list_tools()
async def list_tools() -> list[Tool]:
    """Return 4 MCP tools."""
    return [
        Tool(
            name="search",
            description="Search for directives, tools, or knowledge by query",
            inputSchema={
                "type": "object",
                "properties": {
                    "item_type": {"type": "string", "enum": ["directive", "tool", "knowledge"]},
                    "query": {"type": "string"},
                    "project_path": {"type": "string"},
                    "source": {"type": "string", "enum": ["project", "user", "system", "all"], "default": "project"},
                    "limit": {"type": "integer", "default": 10},
                },
                "required": ["item_type", "query", "project_path"],
            },
        ),
        Tool(
            name="load",
            description="Load item content for inspection or copy between locations",
            inputSchema={
                "type": "object",
                "properties": {
                    "item_type": {"type": "string", "enum": ["directive", "tool", "knowledge"]},
                    "item_id": {"type": "string"},
                    "project_path": {"type": "string"},
                    "source": {"type": "string", "enum": ["project", "user", "system"], "default": "project"},
                    "destination": {"type": "string", "enum": ["project", "user"]},
                },
                "required": ["item_type", "item_id", "project_path"],
            },
        ),
        Tool(
            name="execute",
            description="Execute a directive, tool, or knowledge item",
            inputSchema={
                "type": "object",
                "properties": {
                    "item_type": {"type": "string", "enum": ["directive", "tool", "knowledge"]},
                    "item_id": {"type": "string"},
                    "project_path": {"type": "string"},
                    "parameters": {"type": "object"},
                    "dry_run": {"type": "boolean", "default": False},
                },
                "required": ["item_type", "item_id", "project_path"],
            },
        ),
        Tool(
            name="sign",
            description="Validate and sign an item file",
            inputSchema={
                "type": "object",
                "properties": {
                    "item_type": {"type": "string", "enum": ItemType.ALL},
                    "item_id": {"type": "string"},
                    "project_path": {"type": "string"},
                    "source": {"type": "string", "enum": ["project", "user"], "default": "project"},
                    "parameters": {"type": "object"},
                },
                "required": ["item_type", "item_id", "project_path"],
            },
        ),
    ]

@self.server.call_tool()
async def call_tool(name: str, arguments: dict) -> list[TextContent]:
    """Dispatch to appropriate tool."""
    try:
        if name == "search":
            result = await self.search.handle(**arguments)
        elif name == "load":
            result = await self.load.handle(**arguments)
        elif name == "execute":
            result = await self.execute.handle(**arguments)
        elif name == "sign":
            result = await self.sign.handle(**arguments)
        else:
            result = {"error": f"Unknown tool: {name}"}

        return [TextContent(type="text", text=json.dumps(result))]
    except Exception as e:
        import traceback
        error = {"error": str(e), "traceback": traceback.format_exc()}
        return [TextContent(type="text", text=json.dumps(error, indent=2))]
```

---

## The 3 Data Types

### Directory Structure

```
.ai/
├── directives/              # XML markdown workflows
├── tools/                   # Executable code (Python, JS, YAML, Bash)
├── knowledge/               # Markdown with YAML frontmatter
├── parsers/                 # Data-driven parser implementations
└── tools/rye/               # System tools (bundled with Rye OS)
    ├── core/
    │   ├── primitives/      # Lilux primitive wrappers
    │   ├── runtimes/        # Execution runtimes
    │   └── parsers/         # Bundled parsers
    ├── file-system/         # File ops
    └── agent/threads/       # Thread management
```

### 1. Directives (XML Workflows)

**Format**: Markdown files with embedded XML `<directive>` tags

**Structure**:
```xml
<directive name="directive_name" version="1.0.0">
  <metadata>
    <description>What this does</description>
    <category>category_name</category>
    <author>author_name</author>
    
    <model tier="orchestrator" fallback="general" parallel="true">
      Model tier, fallback, and parallelization options
    </model>
    
    <permissions>
      <execute>
        <tool>rye.file-system.fs_read</tool>
        <tool>custom/security_scanner</tool>
      </execute>
    </permissions>
    
    <cost>
      <context estimated_usage="high" turns="10" spawn_threshold="3">
        50000
      </context>
    </cost>
  </metadata>

  <process>
    <step name="step_name">
      <description>What this step does</description>
      <action><![CDATA[
        Describe action or call tools:
        - Call execute tool with parameters
      ]]></action>
    </step>
  </process>

  <success_criteria>
    <criterion>Measurable success metric 1</criterion>
    <criterion>Measurable success metric 2</criterion>
  </success_criteria>

  <outputs>
    <output name="output_name">Description of output</output>
  </outputs>
</directive>
```

**Execution**: When agent calls `execute` with a directive, it receives the **parsed XML structure**. The agent then **follows the steps** and makes subsequent tool calls.

### 2. Tools (Executable Code)

**Format**: Python, JavaScript, YAML, or Bash with metadata headers

**Metadata Fields**:
```python
__version__ = "1.0.0"
__executor_id__ = "python_runtime"  # Routes through which executor
__tool_type__ = "utility"
__category__ = "file_operations"
```

**Example Python Tool**:
```python
"""
Tool: deploy-service
Version: 1.0.0
Tool Type: python
Executor: python_runtime
Category: deployment
Description: Deploy service to Kubernetes cluster
"""

__version__ = "1.0.0"
__executor_id__ = "python_runtime"

async def main(**kwargs):
    """Execute deployment with provided parameters"""
    service_name = kwargs.get('service_name')
    replicas = kwargs.get('replicas', 3)
    
    # Deployment logic
    print(f"Deploying {service_name} with {replicas} replicas")
    return {"status": "success", "service": service_name}
```

**Execution**: Routes through **PrimitiveExecutor** which builds a 3-layer chain and executes via Lilux primitives.

### 3. Knowledge (Structured Information)

**Format**: Markdown with YAML frontmatter

```markdown
---
id: security-best-practices
title: Security Best Practices
tags: [security, guidelines]
references:
  - https://owasp.org
  - https://cheatsheetseries.owasp.org
---

# Security Best Practices

## Input Validation
- Always validate user input
- Use allowlists when possible
- Never trust client-side validation

## Authentication
- Use strong password hashing (bcrypt, argon2)
- Implement rate limiting on login endpoints
```

**Execution**: When agent calls `execute` with knowledge, receives **parsed content** to inform decisions.

---

## Tool Execution: PrimitiveExecutor

Location: `rye/executor/primitive_executor.py`

### 3-Layer Routing Architecture

```
┌──────────────────────────────────────┐
│  Layer 3: Tools                      │
│  __executor_id__ = "python_runtime"  │
│  (e.g., security_analyzer.py)        │
└──────────────┬───────────────────────┘
               │
               ↓
┌──────────────────────────────────────┐
│  Layer 2: Runtimes                   │
│  __executor_id__ = "subprocess"      │
│  (executes tools in isolated process)│
└──────────────┬───────────────────────┘
               │
               ↓
┌──────────────────────────────────────┐
│  Layer 1: Primitives                 │
│  __executor_id__ = None (or ID)      │
│  Maps to Lilux primitives            │
└──────────────┬───────────────────────┘
               │
               ↓
        ┌──────────────┐
        │ Lilux Runtime│
        └──────────────┘
```

### ChainElement Structure

```python
@dataclass
class ChainElement:
    """Element in the executor chain."""
    item_id: str                          # e.g., "security/analyzer"
    path: Path                            # e.g., ~/.ai/tools/security/analyzer.py
    space: str                            # "project" | "user" | "system"
    tool_type: Optional[str]              # From __tool_type__
    executor_id: Optional[str]            # From __executor_id__
    env_config: Optional[Dict[str, Any]]  # Environment variables
    config_schema: Optional[Dict[str, Any]]
    config: Optional[Dict[str, Any]]
```

### Execution Flow

```python
async def handle(self, **kwargs) -> Dict[str, Any]:
    """Handle execute request."""
    item_type: str = kwargs["item_type"]
    item_id: str = kwargs["item_id"]
    project_path = kwargs["project_path"]
    parameters: Dict[str, Any] = kwargs.get("parameters", {})
    dry_run = kwargs.get("dry_run", False)

    try:
        start = time.time()

        if item_type == ItemType.DIRECTIVE:
            result = await self._run_directive(
                item_id, project_path, parameters, dry_run
            )
        elif item_type == ItemType.TOOL:
            result = await self._run_tool(
                item_id, project_path, parameters, dry_run
            )
        elif item_type == ItemType.KNOWLEDGE:
            result = await self._run_knowledge(item_id, project_path)
        else:
            return {
                "status": "error",
                "error": f"Unknown item type: {item_type}",
            }

        duration_ms = int((time.time() - start) * 1000)
        if "metadata" not in result:
            result["metadata"] = {}
        result["metadata"]["duration_ms"] = duration_ms

        return result

    except Exception as e:
        logger.error(f"Execute error: {e}")
        return {"status": "error", "error": str(e), "item_id": item_id}
```

### Tool Execution Path

```python
async def _run_tool(
    self, item_id: str, project_path: str, parameters: Dict[str, Any], dry_run: bool
) -> Dict[str, Any]:
    """Run a tool via PrimitiveExecutor with chain resolution.

    Execution flow:
        1. Get or create PrimitiveExecutor for project
        2. Build executor chain (tool → runtime → primitive)
        3. Validate chain (space compatibility, I/O matching)
        4. Resolve ENV_CONFIG through chain
        5. Execute via root Lilux primitive
    """
    # Get executor for this project
    executor = self._get_executor(project_path)

    if dry_run:
        # Validate chain without executing
        try:
            chain = await executor._build_chain(item_id)
            if not chain:
                return {"status": "error", "error": f"Tool not found: {item_id}"}

            validation = executor._validate_chain(chain)
            if not validation.valid:
                return {
                    "status": "error",
                    "error": f"Chain validation failed: {'; '.join(validation.issues)}",
                    "item_id": item_id,
                }

            return {
                "status": "validation_passed",
                "message": "Tool chain validation passed (dry run)",
                "item_id": item_id,
                "chain": [executor._chain_element_to_dict(e) for e in chain],
                "validated_pairs": validation.validated_pairs,
            }
        except Exception as e:
            return {"status": "error", "error": str(e), "item_id": item_id}

    # Execute via PrimitiveExecutor
    result: ExecutionResult = await executor.execute(
        item_id=item_id,
        parameters=parameters,
        validate_chain=True,
    )

    if result.success:
        return {
            "status": "success",
            "type": ItemType.TOOL,
            "item_id": item_id,
            "data": result.data,
            "chain": result.chain,
            "metadata": {
                "duration_ms": result.duration_ms,
                **result.metadata,
            },
        }
    else:
        return {
            "status": "error",
            "error": result.error,
            "item_id": item_id,
            "chain": result.chain,
            "metadata": {"duration_ms": result.duration_ms},
        }
```

### Directive Execution Path

```python
async def _run_directive(
    self, item_id: str, project_path: str, parameters: Dict[str, Any], dry_run: bool
) -> Dict[str, Any]:
    """Run a directive - parse and return for agent to follow."""
    file_path = self._find_item(project_path, ItemType.DIRECTIVE, item_id)
    if not file_path:
        return {"status": "error", "error": f"Directive not found: {item_id}"}

    content = file_path.read_text(encoding="utf-8")
    parsed = self.parser_router.parse("markdown_xml", content)

    if "error" in parsed:
        return {"status": "error", "error": parsed.get("error"), "item_id": item_id}

    result = {
        "status": "success",
        "type": ItemType.DIRECTIVE,
        "item_id": item_id,
        "data": parsed,
        "instructions": "Execute the directive as specified now.",
    }

    if dry_run:
        result["status"] = "validation_passed"
        result["message"] = "Directive validation passed (dry run)"

    return result
```

---

## Data-Driven Parsing System

Location: `rye/utils/parser_router.py`

### Parser Architecture

**Key Insight**: Parsers are loaded from the filesystem, enabling customization and override.

```python
class ParserRouter:
    """Routes parsing requests to the appropriate data-driven parser."""

    def __init__(self, project_path: Optional[Path] = None):
        """Initialize parser router."""
        self.project_path = project_path
        self._parsers: Dict[str, Any] = {}

    def get_search_paths(self) -> List[Path]:
        """Get parser search paths in precedence order."""
        paths = []

        # Project parsers (highest priority)
        if self.project_path:
            project_parsers = self.project_path / ".ai" / "parsers"
            if project_parsers.exists():
                paths.append(project_parsers)

        # User parsers
        user_parsers = get_user_space() / "parsers"
        if user_parsers.exists():
            paths.append(user_parsers)

        # System parsers (bundled with rye in .ai/tools/rye/core/parsers/)
        system_parsers = get_system_space() / "tools" / "rye" / "core" / "parsers"
        if system_parsers.exists():
            paths.append(system_parsers)

        return paths

    def _load_parser(self, parser_name: str) -> Optional[Any]:
        """Load a parser module by name."""
        if parser_name in self._parsers:
            return self._parsers[parser_name]

        for search_path in self.get_search_paths():
            parser_file = search_path / f"{parser_name}.py"
            if parser_file.exists():
                try:
                    spec = importlib.util.spec_from_file_location(
                        parser_name, parser_file
                    )
                    if spec and spec.loader:
                        module = importlib.util.module_from_spec(spec)
                        spec.loader.exec_module(module)
                        self._parsers[parser_name] = module
                        logger.debug(f"Loaded parser: {parser_name} from {parser_file}")
                        return module
                except Exception as e:
                    logger.warning(f"Failed to load parser {parser_name}: {e}")
                    continue

        logger.warning(f"Parser not found: {parser_name}")
        return None

    def parse(self, parser_name: str, content: str) -> Dict[str, Any]:
        """
        Parse content using the specified parser.

        Args:
            parser_name: Name of parser (e.g., "markdown_xml", "markdown_frontmatter")
            content: Content to parse

        Returns:
            Parsed data dict, or dict with "error" key on failure
        """
        parser = self._load_parser(parser_name)
        if not parser:
            return {"error": f"Parser not found: {parser_name}"}

        if not hasattr(parser, "parse"):
            return {"error": f"Parser {parser_name} has no parse() function"}

        try:
            return parser.parse(content)
        except Exception as e:
            logger.error(f"Parser {parser_name} failed: {e}")
            return {"error": str(e)}
```

---

## Space Precedence System

**3-Tier Resolution**: project → user → system

Each item type has directories in all three spaces:

```
Priority 1 (Highest): Project Space
  .ai/tools/
  .ai/directives/
  .ai/knowledge/
  .ai/parsers/
  
Priority 2: User Space
  ~/.ai/tools/
  ~/.ai/directives/
  ~/.ai/knowledge/
  ~/.ai/parsers/
  
Priority 3 (Lowest): System Space
  /path/to/rye/installation/.ai/tools/  (bundled)
  /path/to/rye/installation/.ai/parsers/
```

### Item Finding Logic

```python
def _find_item(
    self, project_path: str, item_type: str, item_id: str
) -> Optional[Path]:
    """Find item file by relative path ID searching project > user > system.

    Args:
        item_id: Relative path from .ai/<type>/ without extension.
                e.g., "rye/core/registry/registry" -> .ai/tools/rye/core/registry/registry.py
    """
    type_dir = ItemType.TYPE_DIRS.get(item_type)
    if not type_dir:
        return None

    # Search order: project > user > system
    search_bases = []
    if project_path:
        search_bases.append(get_project_type_path(Path(project_path), item_type))
    search_bases.append(get_user_type_path(item_type))
    search_bases.append(get_system_type_path(item_type))

    # Get extensions data-driven from extractors
    if item_type == ItemType.TOOL:
        extensions = get_tool_extensions(
            Path(project_path) if project_path else None
        )
    else:
        extensions = [".md"]

    for base in search_bases:
        if not base.exists():
            continue
        for ext in extensions:
            file_path = base / f"{item_id}{ext}"
            if file_path.is_file():
                return file_path

    return None
```

---

## Handler Architecture

### DirectiveHandler (`rye/handlers/directive/handler.py`)

```python
class DirectiveHandler:
    """Handler for directive operations."""

    def __init__(
        self, project_path: Optional[str] = None, user_space: Optional[str] = None
    ):
        """Initialize handler."""
        self.project_path = Path(project_path) if project_path else Path.cwd()
        self.user_space = Path(user_space) if user_space else get_user_space()
        self.parser_router = ParserRouter()

    def get_search_paths(self) -> list[Path]:
        """Get directive search paths in precedence order."""
        paths = []

        # Project directives
        project_directives = get_project_type_path(self.project_path, "directive")
        if project_directives.exists():
            paths.append(project_directives)

        # User directives
        user_directives = get_user_type_path("directive")
        if user_directives.exists():
            paths.append(user_directives)

        # System directives
        system_directives = get_system_type_path("directive")
        if system_directives.exists():
            paths.append(system_directives)

        return paths

    def resolve(self, directive_name: str) -> Optional[Path]:
        """Find directive file by name."""
        for search_path in self.get_search_paths():
            for file_path in search_path.rglob(f"{directive_name}.md"):
                if file_path.is_file():
                    return file_path
        return None

    def parse(self, file_path: Path) -> Dict[str, Any]:
        """Parse directive file."""
        content = file_path.read_text(encoding="utf-8")
        return self.parser_router.parse("markdown_xml", content)

    def validate(self, file_path: Path) -> Dict[str, Any]:
        """Validate directive structure."""
        try:
            parsed = self.parse(file_path)
            if "error" in parsed:
                return {"valid": False, "issues": [parsed["error"]]}

            issues = []

            # Check required fields
            if not parsed.get("name"):
                issues.append("Missing directive name attribute")
            if not parsed.get("version"):
                issues.append("Missing version attribute")

            return {"valid": len(issues) == 0, "issues": issues}
        except Exception as e:
            return {"valid": False, "issues": [str(e)]}
```

### ToolHandler (`rye/handlers/tool/handler.py`)

```python
class ToolHandler:
    """Handler for tool operations."""

    def __init__(
        self, project_path: Optional[str] = None, user_space: Optional[str] = None
    ):
        """Initialize handler."""
        self.project_path = Path(project_path) if project_path else Path.cwd()
        self.user_space = Path(user_space) if user_space else get_user_space()

    def get_search_paths(self) -> list[Path]:
        """Get tool search paths in precedence order."""
        paths = []

        # Project tools
        project_tools = get_project_type_path(self.project_path, "tool")
        if project_tools.exists():
            paths.append(project_tools)

        # User tools
        user_tools = get_user_type_path("tool")
        if user_tools.exists():
            paths.append(user_tools)

        # System tools
        system_tools = get_system_type_path("tool")
        if system_tools.exists():
            paths.append(system_tools)

        return paths

    def resolve(self, tool_name: str) -> Optional[Path]:
        """Find tool file by name."""
        extensions = get_tool_extensions(self.project_path)

        for search_path in self.get_search_paths():
            for ext in extensions:
                for file_path in search_path.rglob(f"{tool_name}{ext}"):
                    if file_path.is_file():
                        return file_path
        return None

    def extract_metadata(self, file_path: Path) -> Dict[str, Any]:
        """Extract metadata from tool file using AST parsing."""
        import ast
        import re

        metadata = {
            "name": file_path.stem,
            "path": str(file_path),
            "extension": file_path.suffix,
            "version": None,
            "tool_type": None,
            "executor_id": None,
            "category": None,
        }

        try:
            content = file_path.read_text(encoding="utf-8")

            if file_path.suffix == ".py":
                tree = ast.parse(content)

                for node in tree.body:
                    if isinstance(node, ast.Assign) and len(node.targets) == 1:
                        target = node.targets[0]
                        if isinstance(target, ast.Name) and isinstance(
                            node.value, ast.Constant
                        ):
                            name = target.id
                            value = node.value.value
                            if name == "__version__":
                                metadata["version"] = value
                            elif name == "__tool_type__":
                                metadata["tool_type"] = value
                            elif name == "__executor_id__":
                                metadata["executor_id"] = value
                            elif name == "__category__":
                                metadata["category"] = value

            elif file_path.suffix in (".yaml", ".yml"):
                import yaml

                data = yaml.safe_load(content)
                if isinstance(data, dict):
                    metadata["version"] = data.get("version")
                    metadata["tool_type"] = data.get("tool_type")
                    metadata["executor_id"] = data.get("executor_id")
                    metadata["category"] = data.get("category")

        except Exception as e:
            logger.warning(f"Failed to extract metadata from {file_path}: {e}")

        return metadata

    def validate(self, file_path: Path) -> Dict[str, Any]:
        """Validate tool structure."""
        try:
            content = file_path.read_text(encoding="utf-8")
            issues = []

            if not content.strip():
                issues.append("Tool file is empty")

            metadata = self.extract_metadata(file_path)
            if not metadata.get("version"):
                issues.append("Missing __version__")

            return {"valid": len(issues) == 0, "issues": issues, "metadata": metadata}
        except Exception as e:
            return {"valid": False, "issues": [str(e)]}
```

---

## Key Design Patterns

### 1. Data-Driven Everything
- Tool metadata from `__executor_id__`, `__version__` etc. read via AST
- Parser implementations loaded from filesystem
- No hardcoded routing logic

### 2. 3-Tier Space Precedence
- Enables project customization while keeping system defaults
- Project > User > System for all item types

### 3. Chain Building & Resolution
- Recursive resolution: tool → runtime → primitive
- Each layer adds metadata and environment configuration
- Validation at each step

### 4. Separation of Concerns
- Directives: Returned to agent for orchestration (agent-driven)
- Tools: Executed by PrimitiveExecutor (system-driven)
- Knowledge: Returned for context (informational)

### 5. Cost & Permission Tracking
- Directives declare `<permissions>` and `<cost>` blocks
- SafetyHarness enforces via CapabilityToken
- Thread spawning enables fine-grained control

---

## Example: Complete Tool Execution Flow

### User Request
```
Execute tool "security/code_analyzer" with parameters: { target_path: "/src" }
```

### Step-by-Step Execution

1. **ExecuteTool.handle() receives request**
   ```
   item_type: "tool"
   item_id: "security/code_analyzer"
   parameters: { target_path: "/src" }
   project_path: "/home/user/my_project"
   ```

2. **Find tool file**
   ```
   Search order: 
   - /home/user/my_project/.ai/tools/security/code_analyzer.py ✓ Found
   ```

3. **Extract metadata via AST**
   ```
   __version__ = "1.0.0"
   __executor_id__ = "python_runtime"
   __tool_type__ = "analyzer"
   ```

4. **Build execution chain**
   ```
   Tool: security/code_analyzer
     ├─ executor_id: python_runtime
     └─ Resolve python_runtime:
           Runtime: rye/core/runtimes/python_runtime
             ├─ executor_id: subprocess
             └─ Resolve subprocess:
                   Primitive: rye/core/primitives/subprocess
                     ├─ executor_id: None (is primitive)
                     └─ Maps to: SubprocessPrimitive (from Lilux)
   ```

5. **Validate chain**
   ```
   - Check space compatibility
   - Validate I/O matching
   - Check permissions
   - Collect ENV_CONFIG from all layers
   ```

6. **Execute via Lilux**
   ```
   SubprocessPrimitive.execute(
       command="python /path/to/code_analyzer.py",
       parameters={"target_path": "/src"},
       env_config={...}
   )
   ```

7. **Return result with full trace**
   ```
   {
       "status": "success",
       "data": { findings: [...] },
       "chain": [
           { tool: "security/code_analyzer", space: "project", ... },
           { tool: "rye/core/runtimes/python_runtime", space: "system", ... },
           { tool: "rye/core/primitives/subprocess", space: "system", ... }
       ],
       "metadata": { duration_ms: 1234 }
   }
   ```

---

## Integration Points

- **Lilux Runtime**: Primitives delegate to Lilux for actual execution
- **Integrator Systems**: MCP server works with any MCP-compatible client
- **Registry**: Can publish/pull items from remote registry
- **Thread Control**: SafetyHarness for managed execution of agent threads

This architecture makes Rye OS a **portable, data-driven operating system for AI agent workflows**.
