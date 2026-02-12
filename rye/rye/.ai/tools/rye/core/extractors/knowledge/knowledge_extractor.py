# rye:validated:2026-02-03T07:57:53Z:7a2b32682702d6b516757cf5b9faa82194f310ca77ea12e8e1994607c181b3a4
"""
Knowledge Extractor

Extracts metadata from knowledge (.md) files with YAML frontmatter.
Parses frontmatter for title, tags, and other metadata.
"""

__version__ = "1.0.0"
__tool_type__ = "extractor"
__executor_id__ = None
__category__ = "rye/core/extractors/knowledge"
__tool_description__ = (
    "Knowledge extractor - extracts metadata from YAML frontmatter in markdown files"
)

# File extensions this extractor handles
EXTENSIONS = [".md"]

# Parser type - routes to markdown_frontmatter parser module
PARSER = "markdown_frontmatter"

# Signature format - HTML comment for Markdown
SIGNATURE_FORMAT = {
    "prefix": "<!--",
    "suffix": "-->",
    "after_shebang": False,
}

# Extraction rules using path-based access (parsed data is a dict)
EXTRACTION_RULES = {
    "id": {"type": "path", "key": "id"},
    "title": {"type": "path", "key": "title"},
    "version": {"type": "path", "key": "version"},
    "entry_type": {"type": "path", "key": "entry_type"},
    "category": {"type": "path", "key": "category"},
    "tags": {"type": "path", "key": "tags"},
    "body": {"type": "path", "key": "body"},
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
        "id": {
            "required": True,
            "type": "string",
            "match_filename": True,
        },
        "title": {
            "required": True,
            "type": "string",
        },
        "version": {
            "required": True,
            "type": "semver",
        },
        "entry_type": {
            "required": True,
            "type": "string",
        },
        "category": {
            "required": False,
            "type": "string",
            "match_path": True,
        },
    },
}
