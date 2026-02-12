# rye:validated:2026-02-03T07:58:22Z:caf240a410b32b5338175079256c227e0a7d1f898eac102eda896ac2dba9c6b0
"""
Tool Extractor

Extracts metadata from tool files (.py, .yaml, .json, etc).
Handles Python tools with metadata in comments.
"""

__version__ = "1.0.0"
__tool_type__ = "extractor"
__executor_id__ = None
__category__ = "rye/core/extractors/tool"
__tool_description__ = "Extracts metadata from tool files (.py, .yaml, .json, etc)"

# File extensions this extractor handles
EXTENSIONS = [".py", ".yaml", ".yml", ".json", ".js", ".sh", ".toml"]

# Parser type
PARSER = "python_ast"

# Signature format - line comment with # prefix
SIGNATURE_FORMAT = {
    "prefix": "#",
    "after_shebang": True,
}

# Extraction rules using path-based access (parsed data is a dict)
# For Python files, parser extracts module-level variables
EXTRACTION_RULES = {
    "name": {"type": "filename"},
    "version": {"type": "path", "key": "__version__"},
    "category": {"type": "path", "key": "__category__"},
    "description": {"type": "path", "key": "__tool_description__"},
    "docstring": {"type": "path", "key": "__docstring__"},
    "tool_type": {"type": "path", "key": "__tool_type__"},
    "executor_id": {"type": "path", "key": "__executor_id__"},
}

# Search field weights for scoring (loaded by SearchTool via AST)
SEARCH_FIELDS = {
    "title": 3.0,
    "name": 3.0,
    "description": 2.0,
    "category": 1.5,
    "content": 1.0,
}

# Validation schema - enforces required fields per tool-metadata.md spec
VALIDATION_SCHEMA = {
    "fields": {
        "name": {
            "required": True,
            "type": "string",
            "match_filename": True,
        },
        "category": {
            "required": True,
            "type": "string",
            "match_path": True,
        },
        "tool_type": {
            "required": True,
            "type": "string",
        },
        "version": {
            "required": True,
            "type": "semver",
        },
        "description": {
            "required": True,
            "type": "string",
        },
        "executor_id": {
            "required": True,
            "type": "string",
            "nullable": True,
        },
    },
}
