"""PrimitiveExecutor - Data-driven tool execution with recursive chain resolution.

Routes tools to Lilux primitives based on __executor_id__ metadata.
Loads tools on-demand from .ai/tools/ with 3-tier space precedence.

Architecture:
    Layer 1: Primitives (__executor_id__ = None) - Execute directly via Lilux
    Layer 2: Runtimes (__executor_id__ = "subprocess") - Resolve ENV_CONFIG first
    Layer 3: Tools (__executor_id__ = "python_runtime") - Delegate to runtimes

Caching:
    - Chain cache: Caches resolved execution chains with hash-based invalidation
    - Metadata cache: Caches tool metadata with hash-based invalidation
    - Automatic invalidation when file content changes (via Lilux hash functions)
"""

import ast
import hashlib
import logging
import shlex
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple

from lilux.primitives.subprocess import SubprocessPrimitive, SubprocessResult
from lilux.primitives.http_client import HttpClientPrimitive, HttpResult
from lilux.runtime.env_resolver import EnvResolver

from rye.executor.chain_validator import ChainValidator, ChainValidationResult
from rye.executor.lockfile_resolver import LockfileResolver
from rye.utils.extensions import get_tool_extensions
from rye.utils.integrity import verify_item, IntegrityError
from rye.utils.metadata_manager import MetadataManager
from rye.utils.path_utils import BundleInfo
from rye.constants import AI_DIR, ItemType

logger = logging.getLogger(__name__)

# Maximum allowed chain depth to prevent infinite loops
MAX_CHAIN_DEPTH = 10


@dataclass
class CacheEntry:
    """Cache entry with hash for invalidation."""

    data: Any
    content_hash: str


@dataclass
class ExecutionResult:
    """Result of tool execution."""

    success: bool
    data: Any = None
    error: Optional[str] = None
    duration_ms: float = 0.0
    chain: List[Dict[str, Any]] = field(default_factory=list)
    metadata: Dict[str, Any] = field(default_factory=dict)


@dataclass
class ChainElement:
    """Element in the executor chain."""

    item_id: str
    path: Path
    space: str  # "project", "user", "system"
    tool_type: Optional[str] = None
    executor_id: Optional[str] = None
    env_config: Optional[Dict[str, Any]] = None
    config_schema: Optional[Dict[str, Any]] = None
    config: Optional[Dict[str, Any]] = None
    anchor_config: Optional[Dict[str, Any]] = None
    verify_deps_config: Optional[Dict[str, Any]] = None


class PrimitiveExecutor:
    """Data-driven executor that routes tools to Lilux primitives.

    No hardcoded executor IDs - all resolved from .ai/tools/ filesystem.

    Three-layer routing:
        1. Primitive (__executor_id__ = None): Direct Lilux execution
        2. Runtime (__executor_id__ = "subprocess"): ENV_CONFIG resolution + primitive
        3. Tool (__executor_id__ = "python_runtime"): Delegate to runtime
    """

    # Primitive ID to Lilux primitive class mapping (full path IDs)
    PRIMITIVE_MAP = {
        "rye/core/primitives/subprocess": SubprocessPrimitive,
        "rye/core/primitives/http_client": HttpClientPrimitive,
    }

    def __init__(
        self,
        project_path: Optional[Path] = None,
        user_space: Optional[Path] = None,
        system_space: Optional[Path] = None,
    ):
        """Initialize executor with space paths.

        Args:
            project_path: Project root for .ai/ resolution
            user_space: User space base path (~ or $USER_SPACE)
            system_space: System space base path (site-packages/rye/)
        """
        self.project_path = Path(project_path) if project_path else Path.cwd()
        self.user_space = Path(user_space) if user_space else self._get_user_space()
        if system_space:
            from rye.utils.path_utils import BundleInfo

            self.system_spaces: List[BundleInfo] = [
                BundleInfo(
                    bundle_id="rye-os",
                    version="0.0.0",
                    root_path=Path(system_space),
                    manifest_path=None,
                    source="legacy",
                )
            ]
        else:
            self.system_spaces = self._get_system_spaces()
        # Use first bundle's root_path as the legacy system_space
        self.system_space = self.system_spaces[0].root_path

        self.env_resolver = EnvResolver(project_path=self.project_path)
        self.chain_validator = ChainValidator()
        self.lockfile_resolver = LockfileResolver(
            project_path=self.project_path,
            user_space=self.user_space,
            system_space=self.system_space,
        )

        # Primitive instances (lazy loaded)
        self._primitives: Dict[str, Any] = {}

        # Caches with hash-based invalidation
        self._chain_cache: Dict[
            str, CacheEntry
        ] = {}  # item_id -> CacheEntry(chain, hash)
        self._metadata_cache: Dict[
            str, CacheEntry
        ] = {}  # path -> CacheEntry(metadata, hash)

    async def execute(
        self,
        item_id: str,
        parameters: Optional[Dict[str, Any]] = None,
        validate_chain: bool = True,
        use_lockfile: bool = True,
    ) -> ExecutionResult:
        """Execute a tool by resolving its executor chain recursively.

        Args:
            item_id: Tool identifier (e.g., "git", "python_runtime")
            parameters: Runtime parameters for the tool
            validate_chain: Whether to validate chain before execution
            use_lockfile: Whether to check/create lockfiles

        Returns:
            ExecutionResult with execution details
        """
        import time

        start_time = time.time()
        parameters = parameters or {}
        lockfile_used = False
        lockfile_created = False

        try:
            # 1. Check for existing lockfile
            version = None
            if use_lockfile:
                # Get version from tool metadata first
                tool_path = self._resolve_tool_path(item_id, "project")
                if tool_path:
                    metadata = self._load_metadata_cached(tool_path[0])
                    version = metadata.get("version", "0.0.0")

                    lockfile = self.lockfile_resolver.get_lockfile(item_id, version)
                    if lockfile:
                        logger.debug(f"Using lockfile for {item_id}@{version}")
                        lockfile_used = True
                        content = tool_path[0].read_text(encoding="utf-8")
                        current_integrity = MetadataManager.compute_hash(
                            ItemType.TOOL,
                            content,
                            file_path=tool_path[0],
                            project_path=self.project_path,
                        )
                        if lockfile.root.integrity != current_integrity:
                            return ExecutionResult(
                                success=False,
                                error=(
                                    f"Lockfile integrity mismatch for {item_id}. "
                                    f"Re-sign and delete stale lockfile."
                                ),
                                duration_ms=(time.time() - start_time) * 1000,
                            )

                        for entry in lockfile.resolved_chain:
                            entry_id = entry.get("item_id")
                            entry_space = entry.get("space", "project")
                            entry_integrity = entry.get("integrity")
                            if not entry_id or not entry_integrity:
                                continue
                            resolved = self._resolve_tool_path(entry_id, entry_space)
                            if not resolved:
                                return ExecutionResult(
                                    success=False,
                                    error=(
                                        f"Lockfile chain element not found: {entry_id} "
                                        f"(space: {entry_space}). Delete stale lockfile."
                                    ),
                                    duration_ms=(time.time() - start_time) * 1000,
                                )
                            entry_content = resolved[0].read_text(encoding="utf-8")
                            entry_hash = MetadataManager.compute_hash(
                                ItemType.TOOL,
                                entry_content,
                                file_path=resolved[0],
                                project_path=self.project_path,
                            )
                            if entry_hash != entry_integrity:
                                return ExecutionResult(
                                    success=False,
                                    error=(
                                        f"Lockfile integrity mismatch for chain element "
                                        f"{entry_id}. Re-sign and delete stale lockfile."
                                    ),
                                    duration_ms=(time.time() - start_time) * 1000,
                                )

            # 2. Build the executor chain
            chain = await self._build_chain(item_id)

            if not chain:
                return ExecutionResult(
                    success=False,
                    error=f"Tool not found: {item_id}",
                    duration_ms=(time.time() - start_time) * 1000,
                )

            # 3. Verify integrity of every chain element
            for element in chain:
                verify_item(
                    element.path,
                    ItemType.TOOL,
                    project_path=self.project_path,
                )

            # 4. Validate chain if requested
            if validate_chain:
                validation = self._validate_chain(chain)
                if not validation.valid:
                    return ExecutionResult(
                        success=False,
                        error=f"Chain validation failed: {'; '.join(validation.issues)}",
                        chain=[self._chain_element_to_dict(e) for e in chain],
                        duration_ms=(time.time() - start_time) * 1000,
                    )

            # 4.5 Anchor + verify_deps
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

            # 5. Resolve environment through the chain
            resolved_env = self._resolve_chain_env(chain)

            # 5.5 Apply anchor env mutations
            if anchor_active:
                self._apply_anchor_env(anchor_cfg, resolved_env, anchor_ctx)

            # 5.6 Apply anchor cwd if configured
            if anchor_active and anchor_cfg.get("cwd"):
                cwd = self._template_string(anchor_cfg["cwd"], anchor_ctx)
                parameters = {**(parameters or {}), "cwd": cwd}

            # 6. Execute via the root primitive
            # Inject anchor context vars so {runtime_lib}, {anchor_path},
            # {tool_dir}, {tool_parent} are available for subprocess templating
            parameters = {**anchor_ctx, **(parameters or {})}
            result = await self._execute_chain(chain, parameters, resolved_env)

            # 7. Create lockfile if execution succeeded and none exists
            if use_lockfile and result.get("success") and not lockfile_used and version:
                try:
                    root_element = chain[0]
                    root_content = root_element.path.read_text(encoding="utf-8")
                    integrity = MetadataManager.compute_hash(
                        ItemType.TOOL,
                        root_content,
                        file_path=root_element.path,
                        project_path=self.project_path,
                    )
                    resolved_chain = [self._chain_element_to_dict(e) for e in chain]

                    new_lockfile = self.lockfile_resolver.create_lockfile(
                        tool_id=item_id,
                        version=version,
                        integrity=integrity,
                        resolved_chain=resolved_chain,
                    )
                    self.lockfile_resolver.save_lockfile(
                        new_lockfile, space=chain[0].space
                    )
                    lockfile_created = True
                    logger.info(f"Created lockfile for {item_id}@{version}")
                except Exception as e:
                    logger.warning(f"Failed to create lockfile: {e}")

            duration_ms = (time.time() - start_time) * 1000

            return ExecutionResult(
                success=result.get("success", False),
                data=result.get("data"),
                error=result.get("error"),
                duration_ms=duration_ms,
                chain=[self._chain_element_to_dict(e) for e in chain],
                metadata={
                    "resolved_env_keys": list(resolved_env.keys()),
                    "lockfile_used": lockfile_used,
                    "lockfile_created": lockfile_created,
                },
            )

        except Exception as e:
            logger.exception(f"Execution failed for {item_id}: {e}")
            return ExecutionResult(
                success=False,
                error=str(e),
                duration_ms=(time.time() - start_time) * 1000,
            )

    async def _build_chain(
        self, item_id: str, force_refresh: bool = False
    ) -> List[ChainElement]:
        """Build executor chain by following __executor_id__ recursively.

        Chain is ordered: [tool, runtime, ..., primitive]
        Root of chain (last element) has executor_id = None.

        Uses hash-based caching for performance. Chain is automatically
        invalidated when any file in the chain changes.

        Args:
            item_id: Starting tool identifier
            force_refresh: Skip cache and rebuild from filesystem

        Returns:
            List of ChainElements from tool to primitive
        """
        # Check cache first (unless force refresh)
        if not force_refresh:
            cached_chain = self._get_cached_chain(item_id)
            if cached_chain is not None:
                return cached_chain

        chain: List[ChainElement] = []
        visited: set[str] = set()
        current_id = item_id
        current_space = "project"  # Start resolution from project space

        while current_id:
            # Detect chain depth limit
            if len(chain) >= MAX_CHAIN_DEPTH:
                raise ValueError(
                    f"Chain too deep (max {MAX_CHAIN_DEPTH}): {item_id}. "
                    "Possible circular dependency or excessive nesting."
                )

            # Detect circular dependencies
            if current_id in visited:
                raise ValueError(f"Circular dependency detected: {current_id}")
            visited.add(current_id)

            # Resolve tool path with precedence
            resolved = self._resolve_tool_path(current_id, current_space)
            if not resolved:
                if chain:
                    raise ValueError(f"Executor not found: {current_id}")
                return []  # Tool not found

            path, space = resolved

            # Load metadata (with caching)
            metadata = self._load_metadata_cached(path)

            element = ChainElement(
                item_id=current_id,
                path=path,
                space=space,
                tool_type=metadata.get("tool_type"),
                executor_id=metadata.get("executor_id"),
                env_config=metadata.get("env_config"),
                config_schema=metadata.get("config_schema"),
                config=metadata.get("config"),
                anchor_config=metadata.get("anchor"),
                verify_deps_config=metadata.get("verify_deps"),
            )
            chain.append(element)

            # Check if we've reached a primitive
            if element.executor_id is None:
                break

            # Move to next executor in chain
            current_id = element.executor_id
            current_space = space  # Maintain space context for resolution

        # Cache the resolved chain
        if chain:
            self._cache_chain(item_id, chain)

        return chain

    def _load_metadata_cached(self, path: Path) -> Dict[str, Any]:
        """Load metadata with hash-based caching."""
        # Check cache first
        cached = self._get_cached_metadata(path)
        if cached is not None:
            return cached

        # Cache miss - load from file
        metadata = self._load_metadata(path)

        # Cache for future access
        self._cache_metadata(path, metadata)

        return metadata

    def _resolve_tool_path(
        self, item_id: str, current_space: str = "project"
    ) -> Optional[Tuple[Path, str]]:
        """Resolve tool path by relative path ID using 3-tier space precedence.

        Resolution order: project > user > system
        Each space can shadow tools from lower-precedence spaces.

        Args:
            item_id: Relative path from .ai/tools/ without extension.
                    e.g., "rye/core/registry/registry" -> .ai/tools/rye/core/registry/registry.py
            current_space: Current tool's space (affects resolution rules)

        Returns:
            (path, space) tuple or None if not found
        """
        # Build system entries from all bundles
        system_entries = [
            (bundle.root_path / AI_DIR / "tools", f"system:{bundle.bundle_id}")
            for bundle in self.system_spaces
        ]

        # Determine search order based on current space
        if current_space == "system" or current_space.startswith("system"):
            # System tools can only depend on system tools
            search_order = system_entries
        elif current_space == "user":
            # User tools can depend on user or system tools
            search_order = [
                (self.user_space / AI_DIR / "tools", "user"),
                *system_entries,
            ]
        else:  # project
            # Project tools can depend on any space
            search_order = [
                (self.project_path / AI_DIR / "tools", "project"),
                (self.user_space / AI_DIR / "tools", "user"),
                *system_entries,
            ]

        # Get extensions data-driven from extractors
        extensions = get_tool_extensions(self.project_path)

        for base_path, space in search_order:
            if not base_path.exists():
                continue

            for ext in extensions:
                file_path = base_path / f"{item_id}{ext}"
                if file_path.is_file():
                    return (file_path, space)

        return None

    def _load_metadata(self, path: Path) -> Dict[str, Any]:
        """Load tool metadata from file.

        Extracts:
            - __tool_type__
            - __executor_id__
            - __category__
            - __version__
            - CONFIG_SCHEMA
            - ENV_CONFIG
            - CONFIG

        Args:
            path: Path to tool file

        Returns:
            Metadata dict
        """
        metadata: Dict[str, Any] = {}

        try:
            content = path.read_text(encoding="utf-8")

            if path.suffix == ".py":
                metadata = self._parse_python_metadata(content)
            elif path.suffix in (".yaml", ".yml"):
                metadata = self._parse_yaml_metadata(content)

        except Exception as e:
            logger.warning(f"Failed to load metadata from {path}: {e}")

        return metadata

    def _parse_python_metadata(self, content: str) -> Dict[str, Any]:
        """Parse Python file for metadata using AST.

        Extracts module-level assignments and dict literals.
        """
        metadata: Dict[str, Any] = {}

        try:
            tree = ast.parse(content)

            for node in tree.body:
                if isinstance(node, ast.Assign) and len(node.targets) == 1:
                    target = node.targets[0]
                    if isinstance(target, ast.Name):
                        name = target.id

                        # Simple string/None assignments
                        if isinstance(node.value, ast.Constant):
                            if name == "__version__":
                                metadata["version"] = node.value.value
                            elif name == "__tool_type__":
                                metadata["tool_type"] = node.value.value
                            elif name == "__executor_id__":
                                metadata["executor_id"] = node.value.value
                            elif name == "__category__":
                                metadata["category"] = node.value.value
                            elif name == "__tool_description__":
                                metadata["tool_description"] = node.value.value

                        # Dict assignments (CONFIG_SCHEMA, ENV_CONFIG, CONFIG)
                        elif isinstance(node.value, ast.Dict):
                            if name == "CONFIG_SCHEMA":
                                metadata["config_schema"] = self._ast_dict_to_dict(
                                    node.value
                                )
                            elif name == "ENV_CONFIG":
                                metadata["env_config"] = self._ast_dict_to_dict(
                                    node.value
                                )
                            elif name == "CONFIG":
                                metadata["config"] = self._ast_dict_to_dict(node.value)

        except SyntaxError:
            logger.warning("Failed to parse Python file")

        return metadata

    def _ast_dict_to_dict(self, node: ast.Dict) -> Dict[str, Any]:
        """Convert AST Dict node to Python dict (limited support)."""
        result = {}

        for key, value in zip(node.keys, node.values):
            if isinstance(key, ast.Constant):
                key_str = key.value
                result[key_str] = self._ast_to_value(value)

        return result

    def _ast_to_value(self, node: ast.AST) -> Any:
        """Convert AST node to Python value (limited support)."""
        if isinstance(node, ast.Constant):
            return node.value
        elif isinstance(node, ast.Dict):
            return self._ast_dict_to_dict(node)
        elif isinstance(node, ast.List):
            return [self._ast_to_value(item) for item in node.elts]
        elif isinstance(node, ast.Name):
            # Handle None, True, False
            if node.id == "None":
                return None
            elif node.id == "True":
                return True
            elif node.id == "False":
                return False
        return None

    def _parse_yaml_metadata(self, content: str) -> Dict[str, Any]:
        """Parse YAML file for metadata."""
        try:
            import yaml

            data = yaml.safe_load(content)

            if not isinstance(data, dict):
                return {}

            return {
                "version": data.get("version"),
                "tool_type": data.get("tool_type"),
                "executor_id": data.get("executor_id"),
                "category": data.get("category"),
                "config_schema": data.get("config_schema"),
                "env_config": data.get("env_config"),
                "config": data.get("config"),
                "anchor": data.get("anchor"),
                "verify_deps": data.get("verify_deps"),
            }
        except Exception:
            return {}

    def _validate_chain(self, chain: List[ChainElement]) -> ChainValidationResult:
        """Validate executor chain using ChainValidator."""
        # Convert to format expected by ChainValidator
        chain_dicts = [self._chain_element_to_dict(e) for e in chain]
        return self.chain_validator.validate_chain(chain_dicts)

    def _chain_element_to_dict(self, element: ChainElement) -> Dict[str, Any]:
        """Convert ChainElement to dict for validation/serialization.

        Stores item_id + space (portable) instead of absolute path.
        Includes integrity hash for each element.
        """
        integrity = None
        try:
            content = element.path.read_text(encoding="utf-8")
            integrity = MetadataManager.compute_hash(
                ItemType.TOOL,
                content,
                file_path=element.path,
                project_path=self.project_path,
            )
        except Exception:
            pass

        return {
            "item_id": element.item_id,
            "space": element.space,
            "tool_type": element.tool_type,
            "executor_id": element.executor_id,
            "integrity": integrity,
        }

    def _resolve_chain_env(self, chain: List[ChainElement]) -> Dict[str, str]:
        """Resolve environment variables through the chain.

        Each runtime in the chain can contribute ENV_CONFIG.
        Variables are resolved in order (tool → runtime → primitive).

        Args:
            chain: Executor chain

        Returns:
            Merged resolved environment
        """
        merged_env: Dict[str, str] = {}

        # Process chain in reverse (primitive to tool) for proper override
        for element in reversed(chain):
            if element.env_config:
                resolved = self.env_resolver.resolve(
                    env_config=element.env_config,
                    tool_env=merged_env,
                )
                merged_env.update(resolved)

        return merged_env

    async def _execute_chain(
        self,
        chain: List[ChainElement],
        parameters: Dict[str, Any],
        resolved_env: Dict[str, str],
    ) -> Dict[str, Any]:
        """Execute via the root element in the chain.

        Routes to Lilux primitive classes (tool_type="primitive").

        Args:
            chain: Executor chain (last element is primitive)
            parameters: Runtime parameters
            resolved_env: Resolved environment from chain

        Returns:
            Execution result dict
        """
        if not chain:
            return {"success": False, "error": "Empty chain"}

        # Get the root element (last in chain)
        root_element = chain[-1]

        if root_element.executor_id is not None:
            return {
                "success": False,
                "error": f"Chain root has executor_id: {root_element.item_id}",
            }

        # Build config from chain
        config = self._build_execution_config(chain, resolved_env, parameters)

        # Add executor context
        config["project_path"] = str(self.project_path)
        config["user_space"] = str(self.user_space)
        config["system_space"] = str(self.system_space)

        # Handle primitives (Lilux primitive classes)
        primitive_name = root_element.item_id
        primitive = self._get_primitive(primitive_name)

        if not primitive:
            return {
                "success": False,
                "error": f"Unknown primitive: {primitive_name}",
            }

        # Execute primitive
        # All config values are available for {param} templating in args
        # This includes: tool_path, project_path, params_json, system_space,
        # user_space, and any tool config values (server_config_path, tool_name, etc.)
        enriched_params = {**config, **parameters}

        try:
            result = await primitive.execute(config, enriched_params)

            # Convert primitive result to dict
            if isinstance(result, SubprocessResult):
                # Try to parse stdout as JSON (for tool_runner output)
                error_msg = result.stderr if not result.success else None
                parsed_data = None

                if result.stdout:
                    try:
                        import json

                        parsed_data = json.loads(result.stdout)
                        # Extract error from tool output if present
                        if isinstance(parsed_data, dict):
                            if not parsed_data.get("success", True) and not error_msg:
                                error_msg = (
                                    parsed_data.get("error")
                                    or parsed_data.get("stderr")
                                    or ""
                                )
                    except json.JSONDecodeError:
                        pass

                return {
                    "success": result.success
                    and (parsed_data.get("success", True) if parsed_data else True),
                    "data": parsed_data
                    if parsed_data
                    else {
                        "stdout": result.stdout,
                        "stderr": result.stderr,
                        "return_code": result.return_code,
                    },
                    "error": error_msg,
                }
            elif isinstance(result, HttpResult):
                return {
                    "success": result.success,
                    "data": {
                        "status_code": result.status_code,
                        "body": result.body,
                        "headers": result.headers,
                    },
                    "error": result.error,
                }
            else:
                return {"success": True, "data": result}

        except Exception as e:
            return {"success": False, "error": str(e)}

    def _get_primitive(self, name: str) -> Optional[Any]:
        """Get or create primitive instance by name."""
        if name in self._primitives:
            return self._primitives[name]

        primitive_class = self.PRIMITIVE_MAP.get(name)
        if primitive_class:
            self._primitives[name] = primitive_class()
            return self._primitives[name]

        return None

    def _build_execution_config(
        self,
        chain: List[ChainElement],
        resolved_env: Dict[str, str],
        parameters: Dict[str, Any],
    ) -> Dict[str, Any]:
        """Build execution config by merging chain configs.

        Args:
            chain: Executor chain
            resolved_env: Resolved environment variables
            parameters: Runtime parameters

        Returns:
            Merged config dict for primitive execution
        """
        import json

        config: Dict[str, Any] = {}

        # Merge configs from chain (primitive first, then overrides)
        for element in reversed(chain):
            if element.config:
                config.update(element.config)

        # Merge runtime parameters (highest priority - override chain config)
        # Separate __dunder keys (non-serializable passthrough like __sinks)
        # Exclude keys that collide with primitive config (command, args, timeout, cwd)
        # — those are tool parameters that belong in params_json, not in the
        # subprocess config.  Anchor context keys are safe to merge.
        _CONFIG_KEYS = frozenset(("command", "args"))
        passthrough = {k: v for k, v in parameters.items() if k.startswith("__")}
        config.update({
            k: v for k, v in parameters.items()
            if not k.startswith("__") and k not in _CONFIG_KEYS
        })

        # Apply environment
        config["env"] = resolved_env

        # Inject execution context (generic, available for {param} templating)
        config["project_path"] = str(self.project_path)
        config["user_space"] = str(self.user_space)
        config["system_space"] = str(self.system_space)

        # Inject tool execution context for python_runtime
        # chain[0] is the tool being executed
        if chain:
            tool_element = chain[0]
            config["tool_path"] = str(tool_element.path)

            # Build params_json from parameters (excluding anchor context
            # keys and non-serializable — these are execution metadata,
            # not tool parameters)
            _EXCLUDE_FROM_PARAMS = frozenset((
                "env",
                "tool_path", "tool_dir", "tool_parent",
                "anchor_path", "runtime_lib",
                "project_path", "user_space", "system_space",
            ))
            tool_params = {
                k: v
                for k, v in parameters.items()
                if k not in _EXCLUDE_FROM_PARAMS
                and not k.startswith("__")
            }
            config["params_json"] = json.dumps(tool_params)

        # Template substitution for ${VAR} in config values
        config = self._template_config(config, resolved_env)

        # Strip unresolved single-placeholder values from body
        # (optional provider fields like tools that weren't supplied)
        if isinstance(config.get("body"), dict):
            import re

            config["body"] = {
                k: v
                for k, v in config["body"].items()
                if not (isinstance(v, str) and re.match(r"^\{\w+\}$", v.strip()))
            }

        # Re-inject passthrough keys for primitive access
        config.update(passthrough)

        return config

    def _template_config(
        self, config: Dict[str, Any], env: Dict[str, str]
    ) -> Dict[str, Any]:
        """Substitute ${VAR} and {param} templates in config values.

        Two-pass templating:
        1. ${VAR} - environment variable substitution (with shell escaping)
        2. {param} - config value substitution (recursive until stable)
        """
        import re

        def escape_shell_value(value: Any) -> Any:
            """Escape values that will be used in shell commands."""
            if isinstance(value, str):
                # Only escape if value contains shell-special characters
                if any(
                    c in value
                    for c in [
                        "$",
                        "`",
                        ";",
                        "|",
                        "&",
                        "<",
                        ">",
                        "(",
                        ")",
                        "{",
                        "}",
                        "[",
                        "]",
                        "\\",
                    ]
                ):
                    return shlex.quote(value)
            return value

        # Env var names: uppercase letters, digits, underscores only.
        # This excludes dotted paths like ${state.issues} which belong to
        # the context interpolation system (loaders/interpolation.py).
        _ENV_VAR_RE = re.compile(r"\$\{([A-Z_][A-Z0-9_]*(?::-[^}]*)?)\}")

        def substitute_env(value: Any) -> Any:
            """Substitute ${VAR} with environment values (with escaping)."""
            if isinstance(value, str):

                def replace_var(match: re.Match[str]) -> str:
                    var_expr = match.group(1)
                    if ":-" in var_expr:
                        var_name, default = var_expr.split(":-", 1)
                        raw_value = env.get(var_name, default)
                    else:
                        raw_value = env.get(var_expr, "")
                    return str(escape_shell_value(raw_value)) if raw_value else ""

                return _ENV_VAR_RE.sub(replace_var, value)
            elif isinstance(value, dict):
                return {k: substitute_env(v) for k, v in value.items()}
            elif isinstance(value, list):
                return [substitute_env(item) for item in value]
            return value

        def substitute_params(value: Any, params: Dict[str, Any]) -> Any:
            """Substitute {param} with config values, preserving types for single placeholders.

            When a value is exactly "{param}" (the entire string is one placeholder),
            the original typed value is returned (int, list, dict, etc.).
            When a value contains mixed text like "prefix-{param}", str() is used.
            """
            if isinstance(value, str):
                stripped = value.strip()
                single_match = re.match(r"^\{(\w+)\}$", stripped)
                if single_match:
                    param_name = single_match.group(1)
                    if param_name in params:
                        return params[param_name]
                    return value

                def replace_param(match: re.Match[str]) -> str:
                    param_name = match.group(1)
                    if param_name in params:
                        return str(params[param_name])
                    return match.group(0)

                return re.sub(r"\{([^}]+)\}", replace_param, value)
            elif isinstance(value, dict):
                return {k: substitute_params(v, params) for k, v in value.items()}
            elif isinstance(value, list):
                return [substitute_params(item, params) for item in value]
            return value

        # Pass 1: env var substitution
        result = substitute_env(config)

        # Pass 2: param substitution (iterate until stable, max 3 passes)
        for _ in range(3):
            new_result = substitute_params(result, result)
            if new_result == result:
                break
            result = new_result

        return result

    def _compute_anchor_context(self, chain: List[ChainElement]) -> Dict[str, str]:
        """Compute template variables for anchor resolution."""
        tool_element = chain[0]
        tool_dir = tool_element.path.parent

        # Resolve runtime_lib from the anchor config's lib field
        runtime_lib = ""
        for element in chain:
            if element.anchor_config and element.anchor_config.get("lib"):
                runtime_lib = str(element.path.parent / element.anchor_config["lib"])
                break

        return {
            "tool_path": str(tool_element.path),
            "tool_dir": str(tool_dir),
            "tool_parent": str(tool_dir.parent),
            "anchor_path": str(tool_dir),  # default, overridden by _resolve_anchor_path
            "runtime_lib": runtime_lib,
            "project_path": str(self.project_path),
            "user_space": str(self.user_space),
            "system_space": str(self.system_space),
        }

    def _anchor_applies(self, anchor_cfg: Dict[str, Any], tool_dir: Path) -> bool:
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
        import os as _os

        env_paths = anchor_cfg.get("env_paths", {})
        for var_name, mutations in env_paths.items():
            existing = resolved_env.get(var_name, _os.environ.get(var_name, ""))
            parts = [p for p in existing.split(_os.pathsep) if p] if existing else []

            for path_template in reversed(mutations.get("prepend", [])):
                resolved = self._template_string(path_template, ctx)
                if resolved and resolved not in parts:
                    parts.insert(0, resolved)

            for path_template in mutations.get("append", []):
                resolved = self._template_string(path_template, ctx)
                if resolved and resolved not in parts:
                    parts.append(resolved)

            resolved_env[var_name] = _os.pathsep.join(parts)

    def _template_string(self, template: str, ctx: Dict[str, str]) -> str:
        """Substitute {var} placeholders in a template string."""
        import re

        def replace(match):
            key = match.group(1)
            return ctx.get(key, match.group(0))

        return re.sub(r"\{(\w+)\}", replace, template)

    def _verify_tool_dependencies(
        self, chain: List[ChainElement], anchor_path: Path
    ) -> None:
        """Verify all files in the tool's dependency scope before execution.

        Walks the anchor directory tree, verifying every file matching
        configured extensions via verify_item(). Runs BEFORE subprocess spawn.

        Raises IntegrityError if any file fails verification.
        """
        import os as _os

        # Find verify_deps config from chain (runtime element)
        verify_cfg = None
        for element in chain:
            if element.verify_deps_config:
                verify_cfg = element.verify_deps_config
                break

        if not verify_cfg or not verify_cfg.get("enabled", False):
            return

        extensions = set(verify_cfg.get("extensions", []))
        exclude_dirs = set(
            verify_cfg.get(
                "exclude_dirs",
                [
                    "__pycache__",
                    ".venv",
                    "node_modules",
                    ".git",
                ],
            )
        )
        recursive = verify_cfg.get("recursive", True)

        # Determine base path from scope
        scope = verify_cfg.get("scope", "anchor")
        if scope == "tool_file":
            return  # Only the entry point — already verified in chain
        elif scope == "tool_siblings":
            base = chain[0].path.parent
            recursive = False
        elif scope == "tool_dir":
            base = chain[0].path.parent
        else:  # "anchor"
            base = anchor_path

        base = base.resolve()

        for dirpath, dirnames, filenames in _os.walk(base, followlinks=False):
            # Prune excluded directories
            dirnames[:] = [d for d in dirnames if d not in exclude_dirs]

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

                verify_item(filepath, ItemType.TOOL, project_path=self.project_path)

    def _get_user_space(self) -> Path:
        """Get user space path."""
        from rye.utils.path_utils import get_user_space

        return get_user_space()

    def _get_system_space(self) -> Path:
        """Get system space path (bundled with rye)."""
        from rye.utils.path_utils import get_system_space

        return get_system_space()

    def _get_system_spaces(self) -> List[BundleInfo]:
        """Get all system space roots (core + addon bundles)."""
        from rye.utils.path_utils import get_system_spaces

        return get_system_spaces()

    # -------------------------------------------------------------------------
    # Cache Management
    # -------------------------------------------------------------------------

    def _compute_file_hash(self, path: Path) -> str:
        """Compute SHA256 hash of file content."""
        try:
            content = path.read_bytes()
            return hashlib.sha256(content).hexdigest()
        except Exception:
            return ""

    def _get_cached_metadata(self, path: Path) -> Optional[Dict[str, Any]]:
        """Get cached metadata if file unchanged."""
        path_key = str(path)

        if path_key not in self._metadata_cache:
            return None

        cached = self._metadata_cache[path_key]
        current_hash = self._compute_file_hash(path)

        if current_hash != cached.content_hash:
            # File changed - invalidate
            del self._metadata_cache[path_key]
            return None

        return cached.data

    def _cache_metadata(self, path: Path, metadata: Dict[str, Any]) -> None:
        """Cache metadata with content hash."""
        path_key = str(path)
        content_hash = self._compute_file_hash(path)
        self._metadata_cache[path_key] = CacheEntry(
            data=metadata,
            content_hash=content_hash,
        )

    def _get_cached_chain(self, item_id: str) -> Optional[List[ChainElement]]:
        """Get cached chain if all files unchanged."""
        if item_id not in self._chain_cache:
            return None

        cached = self._chain_cache[item_id]
        chain: List[ChainElement] = cached.data

        # Verify all chain elements still have same hash
        combined_hash = self._compute_chain_hash(chain)

        if combined_hash != cached.content_hash:
            # Some file changed - invalidate
            del self._chain_cache[item_id]
            return None

        return chain

    def _cache_chain(self, item_id: str, chain: List[ChainElement]) -> None:
        """Cache chain with combined hash of all elements."""
        combined_hash = self._compute_chain_hash(chain)
        self._chain_cache[item_id] = CacheEntry(
            data=chain,
            content_hash=combined_hash,
        )

    def _compute_chain_hash(self, chain: List[ChainElement]) -> str:
        """Compute combined hash of all files in chain."""
        combined = ""
        for element in chain:
            file_hash = self._compute_file_hash(element.path)
            combined += file_hash
        return hashlib.sha256(combined.encode()).hexdigest()

    def invalidate_tool(self, item_id: str) -> None:
        """Invalidate cache for a specific tool."""
        if item_id in self._chain_cache:
            del self._chain_cache[item_id]

    def clear_caches(self) -> None:
        """Clear all caches."""
        self._chain_cache.clear()
        self._metadata_cache.clear()

    def get_cache_stats(self) -> Dict[str, int]:
        """Get cache statistics."""
        return {
            "chain_cache_size": len(self._chain_cache),
            "metadata_cache_size": len(self._metadata_cache),
        }
