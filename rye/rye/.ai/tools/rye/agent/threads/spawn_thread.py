# kiwi-mcp:validated:2026-01-27T00:00:00Z:0000000000000000000000000000000000000000000000000000000000000000
# .ai/tools/threads/spawn_thread.py
"""
Thread Spawner Tool

Data-driven tool for spawning OS-level threads with thread ID validation.
Includes sanitization, uniqueness checking, and auto-suggestion for invalid IDs.

This is a harness-agnostic OS primitive - it just spawns threads and registers them.
The actual thread execution logic is provided by the caller (safety_harness, etc.).
"""

__tool_type__ = "python"
__version__ = "1.0.0"
__executor_id__ = "python_runtime"
__category__ = "threads"

import re
import threading
import asyncio
import logging
from pathlib import Path
from typing import Optional, Dict, Any, Callable
import importlib.util
import sys

logger = logging.getLogger(__name__)


def sanitize_thread_id(thread_id: str) -> str:
    """
    Sanitize thread ID according to validation rules.
    
    Rules:
    - Trim whitespace (leading/trailing)
    - Replace internal spaces with underscores
    - Allow only [a-zA-Z0-9_-] characters
    - Ensure non-empty after sanitization
    
    Args:
        thread_id: Raw thread ID string
        
    Returns:
        Sanitized thread ID
        
    Raises:
        ValueError: If thread_id is empty after sanitization
    """
    if not thread_id:
        raise ValueError("Thread ID cannot be empty")
    
    # Trim whitespace
    sanitized = thread_id.strip()
    
    # Replace internal spaces with underscores
    sanitized = re.sub(r'\s+', '_', sanitized)
    
    # Remove any characters not in [a-zA-Z0-9_-]
    sanitized = re.sub(r'[^a-zA-Z0-9_-]', '', sanitized)
    
    # Ensure non-empty after sanitization
    if not sanitized:
        raise ValueError("Thread ID is empty after sanitization")
    
    return sanitized


def suggest_thread_id(thread_id: str) -> str:
    """
    Suggest a valid thread ID from an invalid one.
    
    Args:
        thread_id: Invalid thread ID
        
    Returns:
        Suggested valid thread ID (snake_case pattern)
    """
    # Apply same sanitization
    try:
        return sanitize_thread_id(thread_id)
    except ValueError:
        # If still invalid, try more aggressive conversion
        # Convert to lowercase, replace spaces/special chars with underscores
        suggested = re.sub(r'[^a-zA-Z0-9]', '_', thread_id.lower())
        suggested = re.sub(r'_+', '_', suggested)  # Collapse multiple underscores
        suggested = suggested.strip('_')  # Remove leading/trailing underscores
        
        # Ensure non-empty
        if not suggested:
            suggested = "thread_1"
        
        return suggested


def validate_thread_id(
    thread_id: str,
    project_path: Optional[str] = None,
    check_uniqueness: bool = True
) -> Dict[str, Any]:
    """
    Validate thread ID with sanitization and uniqueness checking.
    
    Args:
        thread_id: Thread ID to validate
        project_path: Project path for registry database location
        check_uniqueness: Whether to check uniqueness in registry
        
    Returns:
        Validation result dict with:
        - valid: bool
        - sanitized: str (sanitized thread_id)
        - error: str (if invalid)
        - suggestion: str (if invalid, suggested replacement)
    """
    result = {
        "valid": False,
        "sanitized": None,
        "error": None,
        "suggestion": None,
    }
    
    # Sanitize
    try:
        sanitized = sanitize_thread_id(thread_id)
        result["sanitized"] = sanitized
    except ValueError as e:
        result["error"] = str(e)
        result["suggestion"] = suggest_thread_id(thread_id)
        return result
    
    # Check uniqueness if requested
    if check_uniqueness:
        try:
            exists = _check_thread_exists(sanitized, project_path)
            if exists:
                result["error"] = f"Thread ID '{sanitized}' already exists in registry"
                result["suggestion"] = f"{sanitized}_2"  # Simple suggestion
                return result
        except Exception as e:
            logger.warning(f"Could not check thread uniqueness: {e}")
            # Continue anyway - uniqueness check is best-effort
    
    result["valid"] = True
    return result


def _check_thread_exists(thread_id: str, project_path: Optional[str] = None) -> bool:
    """
    Check if thread_id exists in thread_registry.
    
    Uses importlib to dynamically load thread_registry tool.
    
    Args:
        thread_id: Thread ID to check
        project_path: Project path for registry database location
        
    Returns:
        True if thread exists, False otherwise
    """
    try:
        # Determine project path
        if not project_path:
            # Try to infer from current working directory
            cwd = Path.cwd()
            if (cwd / ".ai" / "tools" / "threads" / "thread_registry.py").exists():
                project_path = str(cwd)
            else:
                # Default to .ai/threads/registry.db in current directory
                project_path = str(cwd)
        
        project_path = Path(project_path)
        
        # Load thread_registry tool
        registry_path = project_path / ".ai" / "tools" / "threads" / "thread_registry.py"
        
        if not registry_path.exists():
            logger.debug(f"Thread registry not found at {registry_path}, skipping uniqueness check")
            return False
        
        spec = importlib.util.spec_from_file_location(
            'thread_registry',
            registry_path
        )
        if spec is None or spec.loader is None:
            logger.debug("Could not load thread_registry spec")
            return False
        
        thread_registry = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(thread_registry)
        
        # Get ThreadRegistry class
        ThreadRegistry = getattr(thread_registry, 'ThreadRegistry', None)
        if ThreadRegistry is None:
            logger.debug("ThreadRegistry class not found")
            return False
        
        # Initialize registry and check status
        db_path = project_path / ".ai" / "threads" / "registry.db"
        registry = ThreadRegistry(db_path)
        
        status = registry.get_status(thread_id)
        return status is not None
        
    except Exception as e:
        logger.debug(f"Error checking thread existence: {e}")
        return False


async def spawn_thread(
    thread_id: str,
    directive_name: str,
    target_func: Optional[Callable] = None,
    target_args: tuple = (),
    target_kwargs: Optional[Dict[str, Any]] = None,
    project_path: Optional[str] = None,
    register_in_registry: bool = True,
    **kwargs
) -> Dict[str, Any]:
    """
    Spawn OS-level thread with validation and registration.
    
    This is a harness-agnostic OS primitive. The actual thread execution
    is provided by target_func (or handled by the caller).
    
    Args:
        thread_id: Thread identifier (will be sanitized)
        directive_name: Directive name that spawned this thread
        target_func: Optional target function to run in thread
        target_args: Arguments to pass to target_func
        target_kwargs: Keyword arguments to pass to target_func
        project_path: Project path for registry database location
        register_in_registry: Whether to register thread in registry
        **kwargs: Additional parameters (ignored for now)
        
    Returns:
        Result dict with:
        - success: bool
        - thread_id: str (sanitized)
        - status: str
        - error: str (if failed)
        - suggestion: str (if validation failed)
    """
    # Validate thread_id
    validation = validate_thread_id(thread_id, project_path, check_uniqueness=True)
    
    if not validation["valid"]:
        return {
            "success": False,
            "error": validation["error"],
            "suggestion": validation["suggestion"],
            "thread_id": thread_id,
        }
    
    sanitized_id = validation["sanitized"]
    
    # Register in registry if requested
    if register_in_registry:
        try:
            _register_thread(sanitized_id, directive_name, project_path)
        except Exception as e:
            logger.warning(f"Could not register thread in registry: {e}")
            # Continue anyway - registration is best-effort
    
    # Spawn thread if target_func provided
    if target_func:
        thread = threading.Thread(
            target=target_func,
            args=target_args,
            kwargs=target_kwargs or {},
            daemon=True,
            name=sanitized_id
        )
        thread.start()
        logger.info(f"Spawned thread '{sanitized_id}' for directive '{directive_name}'")
    else:
        logger.info(f"Thread '{sanitized_id}' validated (no target_func provided, caller handles spawning)")
    
    return {
        "success": True,
        "thread_id": sanitized_id,
        "status": "spawned",
        "directive_name": directive_name,
    }


def _register_thread(
    thread_id: str,
    directive_name: str,
    project_path: Optional[str] = None
) -> None:
    """
    Register thread in thread_registry.
    
    Args:
        thread_id: Thread ID
        directive_name: Directive name
        project_path: Project path for registry database location
    """
    try:
        # Determine project path
        if not project_path:
            cwd = Path.cwd()
            if (cwd / ".ai" / "tools" / "threads" / "thread_registry.py").exists():
                project_path = str(cwd)
            else:
                project_path = str(cwd)
        
        project_path = Path(project_path)
        
        # Load thread_registry tool
        registry_path = project_path / ".ai" / "tools" / "threads" / "thread_registry.py"
        
        if not registry_path.exists():
            logger.debug(f"Thread registry not found at {registry_path}, skipping registration")
            return
        
        spec = importlib.util.spec_from_file_location(
            'thread_registry',
            registry_path
        )
        if spec is None or spec.loader is None:
            logger.debug("Could not load thread_registry spec")
            return
        
        thread_registry = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(thread_registry)
        
        # Get ThreadRegistry class
        ThreadRegistry = getattr(thread_registry, 'ThreadRegistry', None)
        if ThreadRegistry is None:
            logger.debug("ThreadRegistry class not found")
            return
        
        # Initialize registry and register
        db_path = project_path / ".ai" / "threads" / "registry.db"
        registry = ThreadRegistry(db_path)
        
        registry.register(
            thread_id=thread_id,
            directive_id=directive_name,
            parent_thread_id=None,  # Can be set by caller if needed
        )
        
        logger.debug(f"Registered thread '{thread_id}' in registry")
        
    except Exception as e:
        logger.warning(f"Error registering thread: {e}")
        raise


async def execute(
    thread_id: str,
    directive_name: str,
    target_func: Optional[str] = None,  # Function name as string (not callable)
    target_args: Optional[list] = None,
    target_kwargs: Optional[Dict[str, Any]] = None,
    project_path: Optional[str] = None,
    register_in_registry: bool = True,
    **kwargs
) -> Dict[str, Any]:
    """
    Tool entry point for spawn_thread.
    
    This is the function called by the executor when the tool is invoked.
    
    Args:
        thread_id: Thread identifier
        directive_name: Directive name
        target_func: Optional target function name (not used for now)
        target_args: Optional arguments for target function
        target_kwargs: Optional keyword arguments for target function
        project_path: Project path
        register_in_registry: Whether to register in registry
        **kwargs: Additional parameters
        
    Returns:
        Result dict from spawn_thread()
    """
    # Note: target_func is passed as string, but we can't easily deserialize
    # a callable from a string in a general way. For now, we'll just validate
    # and register, and let the caller handle the actual thread spawning.
    # This matches the "data-driven" pattern - the tool validates and registers,
    # but the actual execution is orchestrated by the harness.
    
    return await spawn_thread(
        thread_id=thread_id,
        directive_name=directive_name,
        target_func=None,  # Caller handles spawning
        target_args=tuple(target_args or ()),
        target_kwargs=target_kwargs,
        project_path=project_path,
        register_in_registry=register_in_registry,
        **kwargs
    )


if __name__ == "__main__":
    import argparse
    import json
    
    parser = argparse.ArgumentParser(description="Thread Spawner Tool")
    parser.add_argument("--thread-id", "--thread_id", dest="thread_id", required=True, help="Thread identifier")
    parser.add_argument("--directive-name", "--directive_name", dest="directive_name", required=True, help="Directive name")
    parser.add_argument("--project-path", "--project_path", dest="project_path", help="Project path")
    parser.add_argument("--register", action="store_true", default=True, help="Register in registry (default: True)")
    parser.add_argument("--no-register", dest="register", action="store_false", help="Don't register in registry")
    parser.add_argument("--validate-only", action="store_true", help="Only validate, don't spawn")
    parser.add_argument("--debug", action="store_true", help="Enable debug logging")
    
    args = parser.parse_args()
    
    if args.debug:
        logging.basicConfig(level=logging.DEBUG)
    else:
        logging.basicConfig(level=logging.INFO)
    
    if args.validate_only:
        # Just validate
        validation = validate_thread_id(args.thread_id, args.project_path, check_uniqueness=True)
        print(json.dumps(validation, indent=2))
        sys.exit(0 if validation["valid"] else 1)
    else:
        # Spawn thread
        result = asyncio.run(execute(
            thread_id=args.thread_id,
            directive_name=args.directive_name,
            project_path=args.project_path,
            register_in_registry=args.register,
        ))
        print(json.dumps(result, indent=2))
        sys.exit(0 if result.get("success") else 1)
