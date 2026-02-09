"""Execute tool - execute directives, tools, or knowledge items.

Routes execution through PrimitiveExecutor for tools, which handles:
    - Three-layer routing: Primitive → Runtime → Tool
    - On-demand tool loading from .ai/tools/
    - Recursive executor chain resolution via __executor_id__
    - ENV_CONFIG resolution for runtimes
    - Space compatibility validation
"""

import logging
import time
from pathlib import Path
from typing import Any, Dict, Optional

from rye.constants import ItemType
from rye.executor import ExecutionResult, PrimitiveExecutor
from rye.utils.extensions import get_tool_extensions
from rye.utils.parser_router import ParserRouter
from rye.utils.path_utils import (
    get_project_type_path,
    get_system_type_path,
    get_user_type_path,
)
from rye.utils.resolvers import get_system_space, get_user_space

logger = logging.getLogger(__name__)


class ExecuteTool:
    """Execute items (directives, tools, knowledge).

    For tools, uses PrimitiveExecutor for data-driven execution
    with recursive chain resolution.
    """

    def __init__(
        self,
        user_space: Optional[str] = None,
        project_path: Optional[str] = None,
    ):
        """Initialize execute tool.

        Args:
            user_space: User space path (~/.ai/)
            project_path: Project root path for .ai/ resolution
        """
        self.user_space = user_space or str(get_user_space())
        self.project_path = project_path
        self.parser_router = ParserRouter()

        # Lazy-loaded executor (created per-project)
        self._executor: Optional[PrimitiveExecutor] = None

    async def handle(self, **kwargs) -> Dict[str, Any]:
        """Handle execute request."""
        item_type: str = kwargs["item_type"]
        item_id: str = kwargs["item_id"]
        project_path = kwargs["project_path"]
        parameters: Dict[str, Any] = kwargs.get("parameters", {})
        dry_run = kwargs.get("dry_run", False)

        logger.debug(f"Execute: {item_type} item_id={item_id}")

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

    def _get_executor(self, project_path: str) -> PrimitiveExecutor:
        """Get or create PrimitiveExecutor for project.

        Creates new executor if project_path changed.
        """
        proj_path = Path(project_path) if project_path else Path.cwd()

        # Check if we need a new executor
        if self._executor is None or self._executor.project_path != proj_path:
            self._executor = PrimitiveExecutor(
                project_path=proj_path,
                user_space=Path(self.user_space),
                system_space=get_system_space(),
            )

        return self._executor

    async def _run_knowledge(self, item_id: str, project_path: str) -> Dict[str, Any]:
        """Run/load knowledge - parse and return content."""
        file_path = self._find_item(project_path, ItemType.KNOWLEDGE, item_id)
        if not file_path:
            return {"status": "error", "error": f"Knowledge entry not found: {item_id}"}

        content = file_path.read_text(encoding="utf-8")
        parsed = self.parser_router.parse("markdown_frontmatter", content)

        if "id" not in parsed:
            parsed["id"] = item_id

        return {
            "status": "success",
            "type": ItemType.KNOWLEDGE,
            "item_id": item_id,
            "data": parsed,
            "instructions": "Use this knowledge to inform your decisions.",
        }

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
