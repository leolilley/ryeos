"""ChainValidator - Validates tool execution chains before execution.

Ensures:
    - Space compatibility (lower precedence cannot depend on higher)
    - I/O compatibility (child outputs match parent inputs)
    - No circular dependencies
    - Version constraints satisfaction
"""

import logging
from dataclasses import dataclass, field
from typing import Any, Dict, List

from packaging import version

logger = logging.getLogger(__name__)


@dataclass
class ChainValidationResult:
    """Result of chain validation."""
    
    valid: bool = True
    issues: List[str] = field(default_factory=list)
    warnings: List[str] = field(default_factory=list)
    validated_pairs: int = 0


class ChainValidator:
    """Validates tool execution chains for integrity and compatibility.
    
    Validation rules:
        1. Space compatibility: Tools can only depend on equal or higher precedence spaces
        2. I/O compatibility: Child outputs must satisfy parent inputs
        3. Version constraints: Parent's child_constraints must be satisfied
        4. No circular dependencies (handled during chain building)
    """
    
    # Space precedence (higher number = higher precedence)
    SPACE_PRECEDENCE = {
        "project": 3,
        "user": 2,
        "system": 1,
    }
    
    def validate_chain(self, chain: List[Dict[str, Any]]) -> ChainValidationResult:
        """Validate entire execution chain.
        
        Chain order: [tool, runtime, ..., primitive]
        Each pair (chain[i], chain[i+1]) is validated.
        
        Args:
            chain: List of chain element dicts with keys:
                - item_id: Tool identifier
                - space: "project", "user", or "system"
                - tool_type: Type of tool
                - executor_id: Delegation target (None for primitives)
                - inputs: Optional list of input types
                - outputs: Optional list of output types
                - version: Optional version string
                - child_constraints: Optional version constraints for dependencies
                
        Returns:
            ChainValidationResult with validation details
        """
        result = ChainValidationResult()
        
        if not chain:
            return result
        
        if len(chain) == 1:
            # Single element chain (primitive) - no pairs to validate
            return result
        
        # Validate each (child, parent) pair
        # In chain order: child → parent (child delegates to parent)
        for i in range(len(chain) - 1):
            child = chain[i]
            parent = chain[i + 1]
            
            self._validate_pair(child, parent, result)
            result.validated_pairs += 1
        
        # Check for space consistency
        self._validate_space_consistency(chain, result)
        
        return result
    
    def _validate_pair(
        self,
        child: Dict[str, Any],
        parent: Dict[str, Any],
        result: ChainValidationResult,
    ) -> None:
        """Validate a (child, parent) pair in the chain.
        
        Child delegates to parent via executor_id.
        """
        # 1. Validate space compatibility
        self._validate_space_compatibility(child, parent, result)
        
        # 2. Validate I/O compatibility
        self._validate_io_compatibility(child, parent, result)
        
        # 3. Validate version constraints
        self._validate_version_constraints(child, parent, result)
    
    def _validate_space_compatibility(
        self,
        child: Dict[str, Any],
        parent: Dict[str, Any],
        result: ChainValidationResult,
    ) -> None:
        """Validate that tools from different spaces are compatible.
        
        Rule: A tool can depend on tools from equal or higher precedence spaces only.
        
        Valid:
            - project → user (project has higher precedence)
            - project → system
            - user → system
            - same space → same space
            
        Invalid:
            - user → project (user cannot depend on project-specific tools)
            - system → project/user (system is immutable)
        """
        child_space = child.get("space", "")
        parent_space = parent.get("space", "")
        
        child_precedence = self.SPACE_PRECEDENCE.get(child_space, 0)
        parent_precedence = self.SPACE_PRECEDENCE.get(parent_space, 0)
        
        # Lower precedence depending on higher precedence: Invalid
        if child_precedence < parent_precedence:
            result.issues.append(
                f"Tool '{child.get('item_id')}' from {child_space} space cannot "
                f"depend on '{parent.get('item_id')}' from {parent_space} space. "
                f"Lower precedence spaces cannot depend on higher precedence spaces."
            )
            result.valid = False
    
    def _validate_io_compatibility(
        self,
        child: Dict[str, Any],
        parent: Dict[str, Any],
        result: ChainValidationResult,
    ) -> None:
        """Validate that child outputs match parent inputs.
        
        If both declare I/O types, ensure compatibility.
        Missing declarations are treated as compatible (warnings only).
        """
        child_outputs = set(child.get("outputs", []))
        parent_inputs = set(parent.get("inputs", []))
        
        # Skip if either side doesn't declare types
        if not child_outputs or not parent_inputs:
            return
        
        # Check if parent's required inputs are satisfied
        missing = parent_inputs - child_outputs
        
        if missing:
            result.issues.append(
                f"I/O mismatch: '{parent.get('item_id')}' requires inputs "
                f"{list(missing)} not provided by '{child.get('item_id')}' "
                f"(outputs: {list(child_outputs)})"
            )
            result.valid = False
    
    def _validate_version_constraints(
        self,
        child: Dict[str, Any],
        parent: Dict[str, Any],
        result: ChainValidationResult,
    ) -> None:
        """Validate version constraints between parent and child.
        
        Parent can specify child_constraints with min_version/max_version.
        """
        parent_constraints = parent.get("child_constraints", {})
        child_id = child.get("item_id", "")
        child_version = child.get("version")
        
        if not parent_constraints or child_id not in parent_constraints:
            return
        
        if not child_version:
            result.warnings.append(
                f"'{child_id}' has no version but '{parent.get('item_id')}' "
                f"specifies version constraints"
            )
            return
        
        constraints = parent_constraints[child_id]
        min_version = constraints.get("min_version")
        max_version = constraints.get("max_version")
        
        if min_version and not self._version_satisfies(child_version, ">=", min_version):
            result.issues.append(
                f"Version constraint failed: '{child_id}' version {child_version} "
                f"< minimum required {min_version}"
            )
            result.valid = False
        
        if max_version and not self._version_satisfies(child_version, "<=", max_version):
            result.issues.append(
                f"Version constraint failed: '{child_id}' version {child_version} "
                f"> maximum allowed {max_version}"
            )
            result.valid = False
    
    def _version_satisfies(self, version_str: str, op: str, constraint: str) -> bool:
         """Check if version satisfies constraint using proper semver.
         
         Supports:
         - Standard semver: 1.0.0, 2.1.3
         - Pre-releases: 1.0.0-alpha, 1.0.0-beta.2
         - Build metadata: 1.0.0+build.123
         """
         try:
             v = version.parse(version_str)
             c = version.parse(constraint)
             
             if op == ">=":
                 return v >= c
             elif op == "<=":
                 return v <= c
             elif op == "==":
                 return v == c
             elif op == ">":
                 return v > c
             elif op == "<":
                 return v < c
             elif op == "!=":
                 return v != c
             else:
                 logger.warning(f"Unknown version operator: {op}")
                 return True
         except version.InvalidVersion:
             logger.warning(f"Invalid version format: {version_str} or {constraint}")
             return True  # Invalid versions pass (warning logged)
    
    def _validate_space_consistency(
        self,
        chain: List[Dict[str, Any]],
        result: ChainValidationResult,
    ) -> None:
        """Validate overall space consistency in the chain.
        
        Additional checks beyond pair validation.
        """
        # Check if system tools are in the middle of a mutable chain
        spaces = [e.get("space") for e in chain]
        
        # Find transitions from system back to mutable
        for i in range(len(spaces) - 1):
            if (spaces[i] or "").startswith("system") and spaces[i + 1] in ("project", "user"):
                result.issues.append(
                    f"Invalid chain: system tool '{chain[i].get('item_id')}' "
                    f"cannot delegate to mutable {spaces[i + 1]} tool "
                    f"'{chain[i + 1].get('item_id')}'"
                )
                result.valid = False
    
    def validate_tool(self, tool: Dict[str, Any]) -> ChainValidationResult:
        """Validate a single tool's metadata.
        
        Lightweight validation without chain context.
        
        Args:
            tool: Tool metadata dict
            
        Returns:
            ChainValidationResult
        """
        result = ChainValidationResult()
        
        # Check required fields
        if not tool.get("item_id"):
            result.issues.append("Missing required field: item_id")
            result.valid = False
        
        # Check space is valid
        space = tool.get("space")
        if space and space not in self.SPACE_PRECEDENCE:
            result.issues.append(f"Invalid space: {space}")
            result.valid = False
        
        # Check executor_id is valid if present
        executor_id = tool.get("executor_id")
        tool_type = tool.get("tool_type")
        
        if tool_type == "primitive" and executor_id is not None:
            result.warnings.append(
                f"Primitive '{tool.get('item_id')}' has executor_id set (should be None)"
            )
        
        if tool_type == "runtime" and executor_id is None:
            result.warnings.append(
                f"Runtime '{tool.get('item_id')}' has no executor_id (should delegate)"
            )
        
        return result
