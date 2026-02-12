"""Search tool - find directives, tools, or knowledge entries.

Implements keyword-based search with:
- Boolean operators (AND, OR, NOT)
- Wildcards (*)
- Phrase search (quotes)
- Field-specific search
- Meta-field filters
- Fuzzy matching (Levenshtein distance)
- Proximity search
- BM25-inspired field-weighted scoring

Field weights and extraction rules are loaded from data-driven extractors
via AST parsing (same pattern as extensions.py / signature_formats.py).
"""

import ast
import logging
import re
from dataclasses import dataclass, field
from datetime import datetime
from fnmatch import fnmatch
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple

from rye.constants import ItemType
from rye.utils.path_utils import (
    get_user_space,
    get_system_space,
    get_project_type_path,
    get_user_type_path,
    get_system_type_path,
    get_extractor_search_paths,
)
from rye.utils.integrity import verify_item, IntegrityError
from rye.utils.parser_router import ParserRouter

logger = logging.getLogger(__name__)

DEFAULT_FIELD_WEIGHTS: Dict[str, float] = {
    "title": 3.0,
    "name": 3.0,
    "description": 2.0,
    "category": 1.5,
    "content": 1.0,
}

_search_fields_cache: Optional[Dict[str, Dict[str, float]]] = None
_extraction_rules_cache: Optional[Dict[str, Dict[str, Any]]] = None
_parser_names_cache: Optional[Dict[str, str]] = None


def _load_extractor_data(
    project_path: Optional[Path] = None,
) -> Tuple[
    Dict[str, Dict[str, float]],
    Dict[str, Dict[str, Any]],
    Dict[str, str],
]:
    """Load SEARCH_FIELDS, EXTRACTION_RULES, and PARSER from all extractors via AST.

    Returns:
        Tuple of (search_fields_by_type, extraction_rules_by_type, parser_by_type)
        keyed by item_type name derived from filename (e.g. "directive").
    """
    search_fields: Dict[str, Dict[str, float]] = {}
    extraction_rules: Dict[str, Dict[str, Any]] = {}
    parser_names: Dict[str, str] = {}
    search_paths = get_extractor_search_paths(project_path)

    for extractors_dir in search_paths:
        if not extractors_dir.exists():
            continue

        for file_path in extractors_dir.glob("**/*_extractor.py"):
            if file_path.name.startswith("_"):
                continue

            item_type_name = file_path.stem.replace("_extractor", "")
            if item_type_name in search_fields:
                continue

            try:
                content = file_path.read_text()
                tree = ast.parse(content)

                for node in tree.body:
                    if not (isinstance(node, ast.Assign) and len(node.targets) == 1):
                        continue
                    target = node.targets[0]
                    if not isinstance(target, ast.Name):
                        continue

                    if target.id == "SEARCH_FIELDS" and isinstance(
                        node.value, ast.Dict
                    ):
                        search_fields[item_type_name] = ast.literal_eval(node.value)

                    elif target.id == "EXTRACTION_RULES" and isinstance(
                        node.value, ast.Dict
                    ):
                        extraction_rules[item_type_name] = ast.literal_eval(node.value)

                    elif target.id == "PARSER" and isinstance(
                        node.value, ast.Constant
                    ):
                        parser_names[item_type_name] = node.value.value

            except Exception as e:
                logger.warning(
                    f"Failed to extract search data from {file_path}: {e}"
                )

    return search_fields, extraction_rules, parser_names


def get_search_fields(
    item_type: str, project_path: Optional[Path] = None
) -> Dict[str, float]:
    """Get search field weights for an item type from extractors."""
    global _search_fields_cache
    if _search_fields_cache is None:
        _search_fields_cache, _, _ = _load_extractor_data(project_path)
    return _search_fields_cache.get(item_type, DEFAULT_FIELD_WEIGHTS)


def get_extraction_rules(
    item_type: str, project_path: Optional[Path] = None
) -> Dict[str, Any]:
    """Get extraction rules for an item type from extractors."""
    global _extraction_rules_cache
    if _extraction_rules_cache is None:
        _, _extraction_rules_cache, _ = _load_extractor_data(project_path)
    return _extraction_rules_cache.get(item_type, {})


def get_parser_name(
    item_type: str, project_path: Optional[Path] = None
) -> Optional[str]:
    """Get parser name for an item type from extractors."""
    global _parser_names_cache
    if _parser_names_cache is None:
        _, _, _parser_names_cache = _load_extractor_data(project_path)
    return _parser_names_cache.get(item_type)


def clear_search_cache():
    """Clear all search-related caches."""
    global _search_fields_cache, _extraction_rules_cache, _parser_names_cache
    _search_fields_cache = None
    _extraction_rules_cache = None
    _parser_names_cache = None


@dataclass
class SearchOptions:
    """Search configuration options."""

    query: str = ""
    item_type: str = ""
    source: str = "project"
    project_path: str = ""
    limit: int = 10
    offset: int = 0
    sort_by: str = "score"
    fields: Dict[str, str] = field(default_factory=dict)
    filters: Dict[str, Any] = field(default_factory=dict)
    fuzzy: Dict[str, Any] = field(default_factory=dict)
    proximity: Dict[str, Any] = field(default_factory=dict)


# ---------------------------------------------------------------------------
# Query AST
# ---------------------------------------------------------------------------


class QueryParser:
    """Parse search queries with boolean operators, phrases, and wildcards."""

    def __init__(self, query: str):
        self.query = query
        self.pos = 0

    def parse(self) -> "QueryNode":
        if not self.query.strip():
            return MatchAllNode()
        return self._parse_or()

    def _parse_or(self) -> "QueryNode":
        left = self._parse_and()
        while self._match_keyword("OR"):
            right = self._parse_and()
            left = OrNode(left, right)
        return left

    def _parse_and(self) -> "QueryNode":
        left = self._parse_not()
        while True:
            if self._match_keyword("AND"):
                right = self._parse_not()
                left = AndNode(left, right)
            elif self._peek_not() or self._peek_term():
                right = self._parse_not()
                left = AndNode(left, right)
            else:
                break
        return left

    def _parse_not(self) -> "QueryNode":
        if self._match_keyword("NOT"):
            return NotNode(self._parse_primary())
        return self._parse_primary()

    def _peek_not(self) -> bool:
        self._skip_whitespace()
        if self.pos >= len(self.query):
            return False
        remaining = (
            self.query[self.pos :].split()[0].upper()
            if self.query[self.pos :].split()
            else ""
        )
        return remaining == "NOT"

    def _parse_primary(self) -> "QueryNode":
        self._skip_whitespace()

        if self.pos >= len(self.query):
            return MatchAllNode()

        if self.query[self.pos] == "(":
            self.pos += 1
            node = self._parse_or()
            self._skip_whitespace()
            if self.pos < len(self.query) and self.query[self.pos] == ")":
                self.pos += 1
            return node

        if self.query[self.pos] == '"':
            return self._parse_phrase()

        return self._parse_term()

    def _parse_phrase(self) -> "QueryNode":
        self.pos += 1
        start = self.pos
        while self.pos < len(self.query) and self.query[self.pos] != '"':
            self.pos += 1
        phrase = self.query[start : self.pos]
        if self.pos < len(self.query):
            self.pos += 1
        return PhraseNode(phrase)

    def _parse_term(self) -> "QueryNode":
        start = self.pos
        while self.pos < len(self.query) and not self.query[self.pos].isspace():
            if self.query[self.pos] in '()"':
                break
            self.pos += 1
        term = self.query[start : self.pos]

        if not term or term.upper() in ("AND", "OR", "NOT"):
            return MatchAllNode()

        if "*" in term:
            return WildcardNode(term)
        return TermNode(term)

    def _match_keyword(self, keyword: str) -> bool:
        self._skip_whitespace()
        if self.pos >= len(self.query):
            return False
        if self.query[self.pos :].upper().startswith(keyword):
            after = self.pos + len(keyword)
            if after >= len(self.query) or self.query[after].isspace():
                self.pos = after
                return True
        return False

    def _peek_term(self) -> bool:
        self._skip_whitespace()
        if self.pos >= len(self.query):
            return False
        if self.query[self.pos] in "()":
            return False
        remaining = (
            self.query[self.pos :].split()[0].upper()
            if self.query[self.pos :].split()
            else ""
        )
        return remaining not in ("AND", "OR", "NOT", "")

    def _skip_whitespace(self):
        while self.pos < len(self.query) and self.query[self.pos].isspace():
            self.pos += 1


class QueryNode:
    """Base class for query AST nodes."""

    def matches(self, text: str, fuzzy_distance: int = 0) -> bool:
        raise NotImplementedError

    def get_terms(self) -> List[str]:
        """Collect raw search terms from the AST (for proximity search)."""
        return []


class MatchAllNode(QueryNode):
    def matches(self, text: str, fuzzy_distance: int = 0) -> bool:
        return True

    def get_terms(self) -> List[str]:
        return []


class TermNode(QueryNode):
    def __init__(self, term: str):
        self.term = term.lower()

    def matches(self, text: str, fuzzy_distance: int = 0) -> bool:
        text_lower = text.lower()
        if self.term in text_lower:
            return True
        if fuzzy_distance > 0:
            return self._fuzzy_match(text_lower, fuzzy_distance)
        return False

    def _fuzzy_match(self, text: str, max_distance: int) -> bool:
        words = re.findall(r"\w+", text)
        for word in words:
            if levenshtein_distance(self.term, word) <= max_distance:
                return True
        return False

    def get_terms(self) -> List[str]:
        return [self.term]


class PhraseNode(QueryNode):
    def __init__(self, phrase: str):
        self.phrase = phrase.lower()

    def matches(self, text: str, fuzzy_distance: int = 0) -> bool:
        return self.phrase in text.lower()

    def get_terms(self) -> List[str]:
        return self.phrase.split()


class WildcardNode(QueryNode):
    def __init__(self, pattern: str):
        self.pattern = pattern.lower()

    def matches(self, text: str, fuzzy_distance: int = 0) -> bool:
        words = re.findall(r"\w+", text.lower())
        for word in words:
            if fnmatch(word, self.pattern):
                return True
        return False

    def get_terms(self) -> List[str]:
        return [self.pattern.replace("*", "")]


class AndNode(QueryNode):
    def __init__(self, left: QueryNode, right: QueryNode):
        self.left = left
        self.right = right

    def matches(self, text: str, fuzzy_distance: int = 0) -> bool:
        return self.left.matches(text, fuzzy_distance) and self.right.matches(
            text, fuzzy_distance
        )

    def get_terms(self) -> List[str]:
        return self.left.get_terms() + self.right.get_terms()


class OrNode(QueryNode):
    def __init__(self, left: QueryNode, right: QueryNode):
        self.left = left
        self.right = right

    def matches(self, text: str, fuzzy_distance: int = 0) -> bool:
        return self.left.matches(text, fuzzy_distance) or self.right.matches(
            text, fuzzy_distance
        )

    def get_terms(self) -> List[str]:
        return self.left.get_terms() + self.right.get_terms()


class NotNode(QueryNode):
    def __init__(self, child: QueryNode):
        self.child = child

    def matches(self, text: str, fuzzy_distance: int = 0) -> bool:
        return not self.child.matches(text, fuzzy_distance)

    def get_terms(self) -> List[str]:
        return []


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def levenshtein_distance(s1: str, s2: str) -> int:
    """Calculate Levenshtein distance between two strings."""
    if len(s1) < len(s2):
        return levenshtein_distance(s2, s1)
    if len(s2) == 0:
        return len(s1)

    prev_row = list(range(len(s2) + 1))
    for i, c1 in enumerate(s1):
        curr_row = [i + 1]
        for j, c2 in enumerate(s2):
            insertions = prev_row[j + 1] + 1
            deletions = curr_row[j] + 1
            substitutions = prev_row[j] + (c1 != c2)
            curr_row.append(min(insertions, deletions, substitutions))
        prev_row = curr_row

    return prev_row[-1]


def proximity_match(text: str, terms: List[str], max_distance: int) -> bool:
    """Check if all terms appear within max_distance words of each other."""
    if len(terms) < 2:
        return True

    words = re.findall(r"\w+", text.lower())
    term_positions: Dict[str, List[int]] = {}

    for term in terms:
        term_lower = term.lower()
        positions = [i for i, w in enumerate(words) if term_lower in w]
        if not positions:
            return False
        term_positions[term_lower] = positions

    all_pos_lists = list(term_positions.values())
    for first_pos in all_pos_lists[0]:
        if all(
            any(abs(first_pos - p) <= max_distance for p in pos_list)
            for pos_list in all_pos_lists[1:]
        ):
            return True

    return False


# ---------------------------------------------------------------------------
# Filter Matcher
# ---------------------------------------------------------------------------


class FilterMatcher:
    """Match items against meta-field filters."""

    @staticmethod
    def matches(item: Dict[str, Any], filters: Dict[str, Any]) -> bool:
        for fld, filter_value in filters.items():
            if not FilterMatcher._match_filter(item, fld, filter_value):
                return False
        return True

    @staticmethod
    def _match_filter(item: Dict[str, Any], fld: str, filter_value: Any) -> bool:
        if fld == "date_from":
            return FilterMatcher._match_date_from(item, filter_value)
        if fld == "date_to":
            return FilterMatcher._match_date_to(item, filter_value)

        item_value = item.get(fld) or item.get("metadata", {}).get(fld)
        if item_value is None:
            return False

        if isinstance(filter_value, list):
            if isinstance(item_value, list):
                return bool(set(filter_value) & set(item_value))
            return item_value in filter_value

        if isinstance(filter_value, str):
            if filter_value.startswith("!"):
                target = filter_value[1:]
                if isinstance(item_value, list):
                    return target not in item_value
                return str(item_value) != target
            if filter_value.startswith(">="):
                return FilterMatcher._compare_version(item_value, filter_value[2:]) >= 0
            if filter_value.startswith("<="):
                return FilterMatcher._compare_version(item_value, filter_value[2:]) <= 0
            if filter_value.startswith(">"):
                return FilterMatcher._compare_version(item_value, filter_value[1:]) > 0
            if filter_value.startswith("<"):
                return FilterMatcher._compare_version(item_value, filter_value[1:]) < 0
            if isinstance(item_value, list):
                return filter_value in item_value
            return str(item_value).lower() == filter_value.lower()

        return item_value == filter_value

    @staticmethod
    def _match_date_from(item: Dict[str, Any], date_str: str) -> bool:
        item_date = item.get("created_at") or item.get("metadata", {}).get(
            "created_at"
        )
        if not item_date:
            return True
        try:
            filter_date = datetime.fromisoformat(date_str.replace("Z", "+00:00"))
            if isinstance(item_date, str):
                item_date = datetime.fromisoformat(item_date.replace("Z", "+00:00"))
            return item_date >= filter_date
        except (ValueError, TypeError):
            return True

    @staticmethod
    def _match_date_to(item: Dict[str, Any], date_str: str) -> bool:
        item_date = item.get("created_at") or item.get("metadata", {}).get(
            "created_at"
        )
        if not item_date:
            return True
        try:
            filter_date = datetime.fromisoformat(date_str.replace("Z", "+00:00"))
            if isinstance(item_date, str):
                item_date = datetime.fromisoformat(item_date.replace("Z", "+00:00"))
            return item_date <= filter_date
        except (ValueError, TypeError):
            return True

    @staticmethod
    def _compare_version(v1: str, v2: str) -> int:
        try:
            parts1 = [int(x) for x in str(v1).split(".")]
            parts2 = [int(x) for x in str(v2).split(".")]
            while len(parts1) < 3:
                parts1.append(0)
            while len(parts2) < 3:
                parts2.append(0)
            for p1, p2 in zip(parts1, parts2):
                if p1 < p2:
                    return -1
                if p1 > p2:
                    return 1
            return 0
        except (ValueError, AttributeError):
            return 0


# ---------------------------------------------------------------------------
# Metadata Extraction (data-driven via extractors + parser router)
# ---------------------------------------------------------------------------


class MetadataExtractor:
    """Extract metadata from files using data-driven extraction rules and parsers."""

    SOURCE_PRIORITY = {"project": 0, "user": 1, "system": 2}

    def __init__(self, project_path: Optional[Path] = None):
        self.project_path = project_path
        self.user_space = get_user_space()
        self._parser_router = ParserRouter(project_path)

    def extract(
        self, file_path: Path, item_type: str, search_dir: Path
    ) -> Optional[Dict[str, Any]]:
        """Extract metadata from a single file.

        Uses the data-driven EXTRACTION_RULES and PARSER from the extractor
        for the given item_type. Falls back to regex-based extraction if the
        parser is unavailable.
        """
        try:
            content = file_path.read_text(encoding="utf-8")
        except Exception as e:
            logger.debug(f"Cannot read {file_path}: {e}")
            return None

        relative_path = file_path.relative_to(search_dir)
        item_id = str(relative_path.with_suffix(""))
        name = file_path.stem

        source = self._detect_source(file_path)

        metadata: Dict[str, Any] = {
            "id": item_id,
            "name": name,
            "title": name,
            "description": "",
            "preview": content[:200],
            "source": source,
            "path": str(file_path),
            "score": 0.0,
            "metadata": {},
        }

        rules = get_extraction_rules(item_type, self.project_path)
        parser_name = get_parser_name(item_type, self.project_path)

        parsed: Optional[Dict[str, Any]] = None
        if parser_name:
            result = self._parser_router.parse(parser_name, content)
            if "error" not in result:
                parsed = result

        if parsed and rules:
            metadata.update(
                self._apply_extraction_rules(parsed, rules, file_path)
            )
        else:
            if item_type == ItemType.DIRECTIVE:
                metadata.update(self._extract_directive_meta(content))
            elif item_type == ItemType.TOOL:
                metadata.update(self._extract_tool_meta(content))
            elif item_type == ItemType.KNOWLEDGE:
                metadata.update(self._extract_knowledge_meta(content))

        try:
            integrity_hash = verify_item(file_path, item_type)
            metadata["signed"] = True
            metadata["integrity"] = integrity_hash
        except IntegrityError:
            metadata["signed"] = False
            metadata["integrity"] = None
        except Exception:
            metadata["signed"] = False
            metadata["integrity"] = None

        return metadata

    def _detect_source(self, file_path: Path) -> str:
        path_str = str(file_path)
        if "site-packages/rye" in path_str:
            return "system"
        if str(self.user_space) in path_str:
            return "user"
        return "project"

    @staticmethod
    def _apply_extraction_rules(
        parsed: Dict[str, Any],
        rules: Dict[str, Any],
        file_path: Path,
    ) -> Dict[str, Any]:
        result: Dict[str, Any] = {"metadata": {}}

        for field_name, rule in rules.items():
            rule_type = rule.get("type", "path")
            value = None

            if rule_type == "filename":
                value = file_path.stem
            elif rule_type == "path":
                key = rule.get("key", field_name)
                value = parsed.get(key)

            if value is None:
                continue

            if field_name in ("name", "title", "description", "category"):
                result[field_name] = value
                if field_name == "category":
                    result["metadata"]["category"] = value
                if field_name == "name":
                    result["title"] = result.get("title") or value
            elif field_name == "version":
                result["metadata"]["version"] = value
            elif field_name == "tags":
                result["metadata"]["tags"] = (
                    value if isinstance(value, list) else [value]
                )
            elif field_name == "body":
                result["preview"] = str(value)[:200]
            elif field_name == "content":
                result["preview"] = str(value)[:200]
            else:
                result["metadata"][field_name] = value

        return result

    # --- Fallback regex extractors (used when parser is unavailable) ---

    @staticmethod
    def _extract_directive_meta(content: str) -> Dict[str, Any]:
        result: Dict[str, Any] = {"title": "", "description": "", "metadata": {}}

        if 'name="' in content:
            match = re.search(r'name="([^"]+)"', content)
            if match:
                result["title"] = match.group(1)
                result["name"] = match.group(1)

        if 'version="' in content:
            match = re.search(r'version="([^"]+)"', content)
            if match:
                result["metadata"]["version"] = match.group(1)

        desc_match = re.search(
            r"<description>(.*?)</description>", content, re.DOTALL
        )
        if desc_match:
            result["description"] = desc_match.group(1).strip()

        category_match = re.search(r"<category>(.*?)</category>", content)
        if category_match:
            result["category"] = category_match.group(1).strip()
            result["metadata"]["category"] = category_match.group(1).strip()

        return result

    @staticmethod
    def _extract_tool_meta(content: str) -> Dict[str, Any]:
        result: Dict[str, Any] = {"metadata": {}}

        if "__version__" in content:
            match = re.search(r'__version__\s*=\s*["\']([^"\']+)["\']', content)
            if match:
                result["metadata"]["version"] = match.group(1)

        if "__category__" in content:
            match = re.search(r'__category__\s*=\s*["\']([^"\']+)["\']', content)
            if match:
                result["category"] = match.group(1)
                result["metadata"]["category"] = match.group(1)

        if "__description__" in content:
            match = re.search(
                r'__description__\s*=\s*["\']([^"\']+)["\']', content
            )
            if match:
                result["description"] = match.group(1)

        docstring_match = re.search(
            r'^"""(.*?)"""', content, re.DOTALL | re.MULTILINE
        )
        if docstring_match and not result.get("description"):
            lines = docstring_match.group(1).strip().split("\n")
            result["description"] = lines[0] if lines else ""

        return result

    @staticmethod
    def _extract_knowledge_meta(content: str) -> Dict[str, Any]:
        result: Dict[str, Any] = {"title": "", "description": "", "metadata": {}}

        if content.startswith("---"):
            lines = content.split("\n")
            for line in lines[1:]:
                if line.strip() == "---":
                    break
                if ":" in line:
                    key, value = line.split(":", 1)
                    key = key.strip()
                    value = value.strip().strip("'\"")

                    if key == "title":
                        result["title"] = value
                    elif key == "description":
                        result["description"] = value
                    elif key == "category":
                        result["category"] = value
                        result["metadata"]["category"] = value
                    elif key == "tags":
                        if value.startswith("[") and value.endswith("]"):
                            tags = [
                                t.strip().strip("'\"")
                                for t in value[1:-1].split(",")
                            ]
                            result["metadata"]["tags"] = tags
                        else:
                            result["metadata"]["tags"] = [value]
                    else:
                        result["metadata"][key] = value

        return result


# ---------------------------------------------------------------------------
# SearchTool
# ---------------------------------------------------------------------------


class SearchTool:
    """Search for items by query with advanced matching.

    Loads field weights from SEARCH_FIELDS in data-driven extractors.
    Uses the 3-tier space system (project > user > system) for resolution.
    """

    def __init__(self, user_space: Optional[str] = None):
        self.user_space = user_space or str(get_user_space())

    async def handle(self, **kwargs) -> Dict[str, Any]:
        """Handle search request matching the spec response schema."""
        opts = SearchOptions(
            query=kwargs.get("query", ""),
            item_type=kwargs["item_type"],
            source=kwargs.get("source", "project"),
            project_path=kwargs["project_path"],
            limit=kwargs.get("limit", 10),
            offset=kwargs.get("offset", 0),
            sort_by=kwargs.get("sort_by", "score"),
            fields=kwargs.get("fields") or {},
            filters=kwargs.get("filters") or {},
            fuzzy=kwargs.get("fuzzy") or {},
            proximity=kwargs.get("proximity") or {},
        )

        logger.debug(
            f"Search: item_type={opts.item_type}, query={opts.query}, "
            f"source={opts.source}"
        )

        try:
            query_ast = QueryParser(opts.query).parse()
            search_paths = self._resolve_search_paths(opts)
            field_weights = get_search_fields(
                opts.item_type, Path(opts.project_path) if opts.project_path else None
            )

            extractor = MetadataExtractor(
                Path(opts.project_path) if opts.project_path else None
            )
            results = self._search_items(
                search_paths, opts, query_ast, field_weights, extractor
            )
            results = self._sort_results(results, opts.sort_by)
            total = len(results)
            results = results[opts.offset : opts.offset + opts.limit]

            return {
                "results": results,
                "total": total,
                "query": opts.query,
                "item_type": opts.item_type,
                "source": opts.source,
                "limit": opts.limit,
                "offset": opts.offset,
                "search_type": "keyword",
            }
        except Exception as e:
            logger.error(f"Search error: {e}", exc_info=True)
            return {"error": str(e), "query": opts.query}

    # ------------------------------------------------------------------
    # Path resolution using 3-tier space system
    # ------------------------------------------------------------------

    def _resolve_search_paths(
        self, opts: SearchOptions
    ) -> List[Tuple[Path, str]]:
        """Resolve (search_dir, source_label) pairs for the given item type."""
        project_path = Path(opts.project_path) if opts.project_path else None
        paths: List[Tuple[Path, str]] = []

        if opts.source in ("project", "all") and project_path:
            d = get_project_type_path(project_path, opts.item_type)
            if d.exists():
                paths.append((d, "project"))

        if opts.source in ("user", "all"):
            d = get_user_type_path(opts.item_type)
            if d.exists():
                paths.append((d, "user"))

        if opts.source in ("system", "all"):
            d = get_system_type_path(opts.item_type)
            if d.exists():
                paths.append((d, "system"))

        return paths

    # ------------------------------------------------------------------
    # Core search loop
    # ------------------------------------------------------------------

    def _search_items(
        self,
        search_paths: List[Tuple[Path, str]],
        opts: SearchOptions,
        query_ast: QueryNode,
        field_weights: Dict[str, float],
        extractor: MetadataExtractor,
    ) -> List[Dict[str, Any]]:
        results: List[Dict[str, Any]] = []
        fuzzy_distance = (
            opts.fuzzy.get("max_distance", 0) if opts.fuzzy.get("enabled") else 0
        )
        prox_enabled = opts.proximity.get("enabled", False)
        prox_distance = opts.proximity.get("max_distance", 5)

        for search_dir, _source_label in search_paths:
            for file_path in search_dir.rglob("*"):
                if not file_path.is_file() or file_path.name.startswith("_"):
                    continue

                item = extractor.extract(file_path, opts.item_type, search_dir)
                if not item:
                    continue

                if not self._matches_query(item, query_ast, opts, fuzzy_distance):
                    continue

                if prox_enabled:
                    terms = query_ast.get_terms()
                    if terms and len(terms) >= 2:
                        searchable = self._get_searchable_text(
                            item, field_weights
                        )
                        if not proximity_match(searchable, terms, prox_distance):
                            continue

                if opts.filters and not FilterMatcher.matches(
                    item, opts.filters
                ):
                    continue

                score = self._score_item(
                    item, opts, field_weights, fuzzy_distance
                )
                item["score"] = round(score, 4)
                item["type"] = opts.item_type
                results.append(item)

        return results

    # ------------------------------------------------------------------
    # Query matching
    # ------------------------------------------------------------------

    def _matches_query(
        self,
        item: Dict[str, Any],
        query_ast: QueryNode,
        opts: SearchOptions,
        fuzzy_distance: int,
    ) -> bool:
        searchable_text = self._get_searchable_text(item, DEFAULT_FIELD_WEIGHTS)
        if not query_ast.matches(searchable_text, fuzzy_distance):
            return False

        for field_name, field_query in opts.fields.items():
            field_value = str(item.get(field_name, ""))
            if field_name == "content":
                field_value = item.get("preview", "")

            field_ast = QueryParser(field_query).parse()
            if not field_ast.matches(field_value, fuzzy_distance):
                return False

        return True

    @staticmethod
    def _get_searchable_text(
        item: Dict[str, Any], field_weights: Dict[str, float]
    ) -> str:
        parts: List[str] = []
        for fld in field_weights:
            if fld == "content":
                val = item.get("preview", "")
            else:
                val = item.get(fld, "")
            if val:
                parts.append(str(val))
        return " ".join(parts)

    # ------------------------------------------------------------------
    # BM25-inspired scoring
    # ------------------------------------------------------------------

    def _score_item(
        self,
        item: Dict[str, Any],
        opts: SearchOptions,
        field_weights: Dict[str, float],
        fuzzy_distance: int,
    ) -> float:
        if not opts.query and not opts.fields:
            return 1.0

        query_ast = (
            QueryParser(opts.query).parse() if opts.query else MatchAllNode()
        )
        total_score = 0.0
        max_score = sum(field_weights.values())

        for field_name, weight in field_weights.items():
            field_value = str(item.get(field_name, ""))
            if field_name == "content":
                field_value = item.get("preview", "")

            if query_ast.matches(field_value, fuzzy_distance):
                total_score += weight

        for field_name, field_query in opts.fields.items():
            field_value = str(item.get(field_name, ""))
            if field_name == "content":
                field_value = item.get("preview", "")
            field_ast = QueryParser(field_query).parse()
            weight = field_weights.get(field_name, 1.0)
            if field_ast.matches(field_value, fuzzy_distance):
                total_score += weight
                max_score += weight

        return min(1.0, total_score / max_score) if max_score > 0 else 0.0

    # ------------------------------------------------------------------
    # Sorting with tie-breaking
    # ------------------------------------------------------------------

    @staticmethod
    def _sort_results(
        results: List[Dict[str, Any]], sort_by: str
    ) -> List[Dict[str, Any]]:
        source_order = {"project": 0, "user": 1, "system": 2}

        def _tie_key(item: Dict[str, Any]) -> Tuple:
            return (
                source_order.get(item.get("source", ""), 9),
                item.get("id", ""),
            )

        if sort_by == "score":
            return sorted(
                results,
                key=lambda x: (-x.get("score", 0), *_tie_key(x)),
            )
        elif sort_by == "date":
            def _date_key(x: Dict[str, Any]) -> str:
                return (
                    x.get("created_at")
                    or x.get("metadata", {}).get("created_at", "")
                    or x.get("metadata", {}).get("updated_at", "")
                    or ""
                )

            return sorted(
                results,
                key=lambda x: (-len(_date_key(x)), _date_key(x)),
                reverse=True,
            )
        elif sort_by == "name":
            return sorted(
                results,
                key=lambda x: (x.get("name", "").lower(), *_tie_key(x)),
            )
        return results
