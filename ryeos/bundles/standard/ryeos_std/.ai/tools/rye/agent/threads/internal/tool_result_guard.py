# rye:signed:2026-04-19T09:49:53Z:7926a818ab98bbece0a1a365e8087a1031190137e34dadddb46c3b24865d940c:5f/zrZXfe5GKs5lnOvDQaivB4wtuytdkfMFJioOLinCIktKQs8Wo9pMTm8c3HzqqAIaF50wbrn5y36lOYptZBw==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb
__version__ = "1.0.0"
__tool_type__ = "python"
__category__ = "rye/agent/threads/internal"
__tool_description__ = "Guard that bounds tool results before they enter conversation context"

import hashlib
import json
from pathlib import Path
from typing import Any, Optional

from module_loader import load_module

_ANCHOR = Path(__file__).resolve().parent.parent


def guard_result(
    result: Any,
    call_id: str,
    tool_name: str,
    thread_id: str,
    project_path: Path,
    context_usage_ratio: float = 0.0,
    no_truncate: bool = False,
) -> Any:
    """Intercept a tool result before it enters conversation context.

    Applies heuristic structural summarization to large results,
    stores full data in an artifact store, and handles content-hash
    deduplication.

    When no_truncate is True (directive declares
    <runtime truncate_tool_results="false" />), the full result is
    returned without summarization.  Deduplication still applies.
    """
    result_str = _serialize(result)
    max_chars = 1200 if context_usage_ratio > 0.75 else 2400

    if len(result_str) <= max_chars:
        return result

    artifact_mod = load_module("persistence/artifact_store", anchor=_ANCHOR)
    store = artifact_mod.get_artifact_store(thread_id, project_path)

    content_hash = _hash_result(result)
    existing_call_id = store.has_content(content_hash)
    if existing_call_id is not None:
        return {
            "status": "success",
            "note": f"Identical to previous result (artifact {existing_call_id}). Content reused from context.",
            "artifact_ref": content_hash,
        }

    artifact_ref = store.store(call_id, tool_name, result)

    if no_truncate:
        return result

    summary = _summarize_result(result, max_chars)
    summary["_artifact_ref"] = artifact_ref
    summary["_artifact_note"] = "Full result stored as artifact. Use artifact ref to retrieve."
    return summary


def check_dedupe(
    result: Any,
    thread_id: str,
    project_path: Path,
) -> Optional[str]:
    """Check if an identical result is already stored as an artifact.

    Hashes the result (json.dumps with sort_keys=True, default=str,
    then sha256) and checks the artifact store for that hash.

    Returns the existing call_id or None.
    """
    artifact_mod = load_module("persistence/artifact_store", anchor=_ANCHOR)
    store = artifact_mod.get_artifact_store(thread_id, project_path)
    content_hash = _hash_result(result)
    return store.has_content(content_hash)


def _serialize(result: Any) -> str:
    if isinstance(result, str):
        return result
    try:
        return json.dumps(result, default=str)
    except (TypeError, ValueError):
        return str(result)


def _hash_result(result: Any) -> str:
    raw = json.dumps(result, sort_keys=True, default=str)
    return hashlib.sha256(raw.encode()).hexdigest()


def _summarize_result(result: Any, max_chars: int) -> dict:
    """Heuristic structural summarization of a tool result."""
    if isinstance(result, str):
        if len(result) > max_chars:
            return {"content": result[:max_chars] + "[... truncated]"}
        return {"content": result}

    if not isinstance(result, dict):
        s = _serialize(result)
        if len(s) > max_chars:
            return {"content": s[:max_chars] + "[... truncated]"}
        return {"content": s}

    summary: dict = {}

    for key in ("status", "error", "warnings", "success"):
        if key in result:
            summary[key] = result[key]

    data = result.get("data")
    if isinstance(data, dict):
        collection_key = None
        collection_list = None

        for k, v in data.items():
            if isinstance(v, list) and len(v) > 0 and isinstance(v[0], dict):
                collection_key = k
                collection_list = v
                break

        if collection_key is not None:
            summary[f"{collection_key}_count"] = len(collection_list)
            preview = []
            for item in collection_list[:3]:
                trimmed = {}
                for ik, iv in item.items():
                    if isinstance(iv, (str, int, float, bool)):
                        if isinstance(iv, str) and len(iv) > 200:
                            trimmed[ik] = iv[:200] + "..."
                        else:
                            trimmed[ik] = iv
                preview.append(trimmed)
            summary[f"{collection_key}_preview"] = preview

        for k, v in data.items():
            if k == collection_key:
                continue
            if isinstance(v, str):
                if k == "content" and len(v) > 500:
                    summary[k] = v[:500] + f"... [truncated, {len(v)} chars total]"
                elif len(v) > 200:
                    summary[k] = v[:200] + "..."
                else:
                    summary[k] = v
            elif isinstance(v, (int, float, bool, type(None))):
                summary[k] = v
    elif data is None:
        for k, v in result.items():
            if k in ("status", "error", "warnings", "success"):
                continue
            if isinstance(v, str):
                if k == "content" and len(v) > 500:
                    summary[k] = v[:500] + f"... [truncated, {len(v)} chars total]"
                elif len(v) > 200:
                    summary[k] = v[:200] + "..."
                else:
                    summary[k] = v
            elif isinstance(v, (int, float, bool, type(None))):
                summary[k] = v
            elif isinstance(v, list) and len(v) > 0 and isinstance(v[0], dict):
                summary[f"{k}_count"] = len(v)
                preview = []
                for item in v[:3]:
                    trimmed = {}
                    for ik, iv in item.items():
                        if isinstance(iv, (str, int, float, bool)):
                            if isinstance(iv, str) and len(iv) > 200:
                                trimmed[ik] = iv[:200] + "..."
                            else:
                                trimmed[ik] = iv
                    preview.append(trimmed)
                summary[f"{k}_preview"] = preview

    return summary
