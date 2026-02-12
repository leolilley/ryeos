# rye:validated:2026-02-03T07:57:53Z:4d27a8c29a3971d7a04a0de441d1f90033802a5d03e6780073c22f73c4eeebdb
"""
Directive Extractor

Extracts metadata from directive (.md) files containing XML structure.
Defines extraction rules and validation schema for XML directives.
"""

__version__ = "1.0.0"
__tool_type__ = "extractor"
__executor_id__ = None
__category__ = "rye/core/extractors/directive"
__tool_description__ = (
    "Directive extractor - extracts metadata from XML directive files"
)

# File extensions this extractor handles
EXTENSIONS = [".md"]

# Parser type - routes to markdown_xml parser module
PARSER = "markdown_xml"

# Signature format - HTML comment for Markdown
SIGNATURE_FORMAT = {
    "prefix": "<!--",
    "suffix": "-->",
    "after_shebang": False,
}

# Extraction rules using path-based access (parsed data is a dict)
EXTRACTION_RULES = {
    "name": {"type": "path", "key": "name"},
    "version": {"type": "path", "key": "version"},
    "description": {"type": "path", "key": "description"},
    "category": {"type": "path", "key": "category"},
    "author": {"type": "path", "key": "author"},
    "model": {"type": "path", "key": "model"},
    "permissions": {"type": "path", "key": "permissions"},
    "inputs": {"type": "path", "key": "inputs"},
    "steps": {"type": "path", "key": "steps"},
    "outputs": {"type": "path", "key": "outputs"},
    "templates": {"type": "path", "key": "templates"},
    "content": {"type": "path", "key": "content"},
}

# Search field weights for scoring (loaded by SearchTool via AST)
SEARCH_FIELDS = {
    "title": 3.0,
    "name": 3.0,
    "description": 2.0,
    "category": 1.5,
    "content": 1.0,
}

# Validation schema - defines required fields and their types
VALIDATION_SCHEMA = {
    "fields": {
        "name": {
            "required": True,
            "type": "string",
            "format": "snake_case",
            "match_filename": True,
        },
        "version": {
            "required": True,
            "type": "semver",
        },
        "description": {
            "required": True,
            "type": "string",
        },
        "category": {
            "required": True,
            "type": "string",
            "match_path": True,
        },
        "author": {
            "required": True,
            "type": "string",
        },
        "model": {
            "required": False,
            "type": "object",
            "nested": {
                "tier": {
                    "required": False,
                    "type": "string",
                },
                "fallback": {
                    "required": False,
                    "type": "string",
                },
                "parallel": {
                    "required": False,
                    "type": "enum",
                    "values": ["true", "false"],
                },
                "id": {
                    "required": False,
                    "type": "string",
                },
            },
        },
        "permissions": {
            "required": False,
            "type": "array",
            "item_type": "object",
            "item_required": ["tag"],
        },
    },
}
