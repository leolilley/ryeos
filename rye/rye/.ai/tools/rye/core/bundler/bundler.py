# rye:signed:2026-02-14T01:02:01Z:a68c50be4b53acdd0814301d05e0d9b03698a2c93f74fdf8451a055578382e55:Eg5cXHvXU1tx8GzeXXMnDKaxy3FIwI3XyrGqtvDY4-uC8SAxsrDyVlt9nBCa09tLpT_TIOD53iZ0Iflcz8LbCw==:440443d0858f0199
"""
Bundler tool - create, verify, inspect, and list bundle manifests.

A bundle is a group of directives, tools, and knowledge items with a signed
manifest that covers all files (including assets that can't have inline
signatures).  Bundles are NOT a separate ItemType; they are managed entirely
by this core tool.

Manifest location: .ai/bundles/{bundle_id}/manifest.yaml
Signature format:  # rye:signed:TIMESTAMP:HASH:SIG:FP  (line 1)

Actions:
  create  - Walk item directories, hash every file, sign and write manifest
  verify  - Verify manifest signature + per-file hashes
  inspect - Parse manifest without verification
  list    - List all bundles under .ai/bundles/
"""

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python_script_runtime"
__category__ = "rye/core/bundler"
__tool_description__ = "Create, verify, and inspect bundle manifests"

import asyncio
import hashlib
import json
import re
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Dict, List, Optional

import yaml

from rye.constants import AI_DIR

TOOL_METADATA = {
    "name": "bundler",
    "description": "Create, verify, and inspect bundle manifests",
    "version": __version__,
    "protected": True,
}

ACTIONS = ["create", "verify", "inspect", "list"]

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "action": {
            "type": "string",
            "enum": ACTIONS,
            "description": "Bundle operation to perform",
        },
        "bundle_id": {
            "type": "string",
            "description": "Bundle identifier, e.g., apps/task-manager",
        },
        "version": {
            "type": "string",
            "description": "Semantic version for create",
        },
        "entrypoint": {
            "type": "string",
            "description": "Directive item_id for bundle entrypoint",
        },
        "description": {
            "type": "string",
            "description": "Bundle description",
        },
    },
    "required": ["action"],
}

# Signature regex for detecting inline-signed files
_SIGNED_RE = re.compile(r"(?:<!--|#|//) rye:signed:")

# Item type directory names
_TYPE_DIRS = {
    "directive": "directives",
    "tool": "tools",
    "knowledge": "knowledge",
}


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _sha256_file(path: Path) -> str:
    """Compute SHA256 hex digest of a file."""
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(8192), b""):
            h.update(chunk)
    return h.hexdigest()


def _classify_file(rel_path: str) -> str:
    """Classify a file into directive/tool/knowledge/asset by its relative path."""
    for item_type, dir_name in _TYPE_DIRS.items():
        if rel_path.startswith(f"{AI_DIR}/{dir_name}/"):
            return item_type
    return "asset"


def _has_inline_signature(path: Path) -> bool:
    """Check whether a file has an inline rye:signed: signature."""
    try:
        head = path.read_text(encoding="utf-8", errors="replace")[:512]
        return bool(_SIGNED_RE.search(head))
    except Exception:
        return False


def _load_collect_config(project_path: Path) -> Dict[str, Any]:
    """Load collect config via standard 3-tier tool resolution.

    Resolves rye/core/bundler/collect.yaml with precedence:
      project (.ai/tools/) → user (~/.ai/tools/) → system (site-packages)

    The collect.yaml is a signed tool YAML like any other, user-extendable
    by placing an override at the same path in user or project space.
    """
    from rye.utils.path_utils import get_user_space, get_system_space

    config_name = "rye/core/bundler/collect.yaml"
    search_order = [
        project_path / AI_DIR / "tools" / config_name,
        get_user_space() / AI_DIR / "tools" / config_name,
        get_system_space() / AI_DIR / "tools" / config_name,
    ]

    for path in search_order:
        if path.exists():
            try:
                data = yaml.safe_load(path.read_text(encoding="utf-8"))
                if isinstance(data, dict):
                    return data
            except Exception:
                continue

    return {}


def _collect_bundle_files(
    project_path: Path, bundle_id: str
) -> List[Dict[str, Any]]:
    """Walk all item directories for a bundle and collect file metadata.

    Exclusion rules are loaded from config/collect.yaml with 3-tier merging
    (system → user → project). No language-specific logic here.
    """
    collect_config = _load_collect_config(project_path)
    exclude_dirs = set(collect_config.get("exclude_dirs", []))

    files: List[Dict[str, Any]] = []

    # Standard item type directories
    for dir_name in _TYPE_DIRS.values():
        bundle_dir = project_path / AI_DIR / dir_name / bundle_id
        if not bundle_dir.is_dir():
            continue

        for file_path in sorted(bundle_dir.rglob("*")):
            if not file_path.is_file():
                continue
            if any(d in file_path.parts for d in exclude_dirs):
                continue
            rel = str(file_path.relative_to(project_path))
            files.append({
                "path": rel,
                "sha256": _sha256_file(file_path),
                "inline_signed": _has_inline_signature(file_path),
            })

    # Plans and lockfiles (use slug: bundle_id with / -> _)
    bundle_slug = bundle_id.replace("/", "_")

    plans_dir = project_path / AI_DIR / "plans" / bundle_slug
    if plans_dir.is_dir():
        for file_path in sorted(plans_dir.rglob("*")):
            if not file_path.is_file():
                continue
            rel = str(file_path.relative_to(project_path))
            files.append({
                "path": rel,
                "sha256": _sha256_file(file_path),
                "inline_signed": _has_inline_signature(file_path),
            })

    lockfiles_dir = project_path / AI_DIR / "lockfiles"
    if lockfiles_dir.is_dir():
        prefix = f"{bundle_slug}_"
        for file_path in sorted(lockfiles_dir.iterdir()):
            if file_path.is_file() and file_path.name.startswith(prefix):
                rel = str(file_path.relative_to(project_path))
                files.append({
                    "path": rel,
                    "sha256": _sha256_file(file_path),
                    "inline_signed": _has_inline_signature(file_path),
                })

    return files


def _files_by_type(files: List[Dict[str, Any]]) -> Dict[str, int]:
    """Count files by classified type."""
    counts: Dict[str, int] = {"directive": 0, "tool": 0, "knowledge": 0, "asset": 0}
    for f in files:
        counts[_classify_file(f["path"])] += 1
    return counts


def _sign_manifest(content: str) -> str:
    """Sign manifest YAML content and prepend signature line.

    Uses the same Ed25519 signing infrastructure as all other rye items.
    """
    from rye.utils.metadata_manager import compute_content_hash, generate_timestamp
    from lilux.primitives.signing import ensure_keypair, sign_hash, compute_key_fingerprint
    from rye.utils.trust_store import TrustStore
    from rye.utils.path_utils import get_user_space

    content_hash = compute_content_hash(content)
    timestamp = generate_timestamp()

    key_dir = get_user_space() / AI_DIR / "keys"
    private_pem, public_pem = ensure_keypair(key_dir)

    ed25519_sig = sign_hash(content_hash, private_pem)
    pubkey_fp = compute_key_fingerprint(public_pem)

    trust_store = TrustStore()
    if not trust_store.is_trusted(pubkey_fp):
        trust_store.add_key(public_pem, label="self")

    sig_line = f"# rye:signed:{timestamp}:{content_hash}:{ed25519_sig}:{pubkey_fp}\n"
    return sig_line + content


def _parse_manifest(manifest_path: Path) -> Dict[str, Any]:
    """Parse a manifest YAML, skipping the signature line."""
    text = manifest_path.read_text(encoding="utf-8")
    # Strip leading signature line(s) before YAML parsing
    lines = text.split("\n")
    yaml_lines = [l for l in lines if not l.startswith("# rye:signed:")]
    return yaml.safe_load("\n".join(yaml_lines)) or {}


# ---------------------------------------------------------------------------
# Actions
# ---------------------------------------------------------------------------

async def _create(
    project_path: Path, params: Dict[str, Any]
) -> Dict[str, Any]:
    """Create a signed bundle manifest."""
    bundle_id = params.get("bundle_id")
    if not bundle_id:
        return {"error": "bundle_id is required for create"}

    version = params.get("version", "0.1.0")
    entrypoint = params.get("entrypoint", "")
    description = params.get("description", "")

    files = _collect_bundle_files(project_path, bundle_id)
    if not files:
        return {
            "error": f"No files found for bundle '{bundle_id}'",
            "searched": [
                f"{AI_DIR}/{d}/{bundle_id}/" for d in _TYPE_DIRS.values()
            ],
        }

    manifest = {
        "bundle": {
            "id": bundle_id,
            "version": version,
            "entrypoint": entrypoint,
            "description": description,
            "created_at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        },
        "files": {
            f["path"]: {
                "sha256": f["sha256"],
                "inline_signed": f["inline_signed"],
            }
            for f in files
        },
    }

    manifest_yaml = yaml.dump(manifest, default_flow_style=False, sort_keys=False)
    signed_yaml = _sign_manifest(manifest_yaml)

    manifest_dir = project_path / AI_DIR / "bundles" / bundle_id
    manifest_dir.mkdir(parents=True, exist_ok=True)
    manifest_path = manifest_dir / "manifest.yaml"
    manifest_path.write_text(signed_yaml, encoding="utf-8")

    return {
        "status": "created",
        "manifest_path": str(manifest_path.relative_to(project_path)),
        "file_count": len(files),
        "files_by_type": _files_by_type(files),
    }


async def _verify(
    project_path: Path, params: Dict[str, Any]
) -> Dict[str, Any]:
    """Verify a bundle manifest and its files."""
    bundle_id = params.get("bundle_id")
    if not bundle_id:
        return {"error": "bundle_id is required for verify"}

    manifest_path = project_path / AI_DIR / "bundles" / bundle_id / "manifest.yaml"
    if not manifest_path.exists():
        return {"error": f"Manifest not found: {manifest_path}"}

    # 1. Verify manifest's own inline signature
    from rye.utils.integrity import verify_item, IntegrityError
    from rye.constants import ItemType

    manifest_valid = True
    manifest_error = None
    try:
        verify_item(manifest_path, ItemType.TOOL, project_path=project_path)
    except IntegrityError as e:
        manifest_valid = False
        manifest_error = str(e)

    # 2. Verify each file's hash
    data = _parse_manifest(manifest_path)
    file_entries = data.get("files", {})

    files_ok = 0
    files_missing: List[str] = []
    files_tampered: List[str] = []

    for rel_path, meta in file_entries.items():
        file_path = project_path / rel_path
        if not file_path.exists():
            files_missing.append(rel_path)
            continue

        actual_hash = _sha256_file(file_path)
        if actual_hash != meta["sha256"]:
            files_tampered.append(rel_path)
            continue

        # If file claims inline signature, verify it too
        if meta.get("inline_signed"):
            item_type = _classify_file(rel_path)
            if item_type in ("directive", "tool", "knowledge"):
                try:
                    verify_item(
                        file_path,
                        item_type,
                        project_path=project_path,
                    )
                except IntegrityError:
                    files_tampered.append(rel_path)
                    continue

        files_ok += 1

    result: Dict[str, Any] = {
        "status": "verified" if manifest_valid and not files_missing and not files_tampered else "failed",
        "manifest_valid": manifest_valid,
        "files_checked": len(file_entries),
        "files_ok": files_ok,
        "files_missing": files_missing,
        "files_tampered": files_tampered,
    }
    if manifest_error:
        result["manifest_error"] = manifest_error
    return result


async def _inspect(
    project_path: Path, params: Dict[str, Any]
) -> Dict[str, Any]:
    """Inspect a bundle manifest without verification."""
    bundle_id = params.get("bundle_id")
    if not bundle_id:
        return {"error": "bundle_id is required for inspect"}

    manifest_path = project_path / AI_DIR / "bundles" / bundle_id / "manifest.yaml"
    if not manifest_path.exists():
        return {"error": f"Manifest not found: {manifest_path}"}

    data = _parse_manifest(manifest_path)
    file_entries = data.get("files", {})

    files_list = [
        {"path": path, **meta}
        for path, meta in file_entries.items()
    ]

    classified = _files_by_type(
        [{"path": f["path"]} for f in files_list]
    )

    return {
        "bundle": data.get("bundle", {}),
        "files": files_list,
        "file_count": len(files_list),
        "files_by_type": classified,
    }


async def _list(
    project_path: Path, params: Dict[str, Any]
) -> Dict[str, Any]:
    """List all bundles under .ai/bundles/."""
    bundles_dir = project_path / AI_DIR / "bundles"
    if not bundles_dir.is_dir():
        return {"bundles": []}

    bundles: List[Dict[str, Any]] = []
    for manifest_path in sorted(bundles_dir.rglob("manifest.yaml")):
        try:
            data = _parse_manifest(manifest_path)
            bundle_meta = data.get("bundle", {})
            rel = str(manifest_path.relative_to(project_path))
            bundles.append({
                "bundle_id": bundle_meta.get("id", ""),
                "version": bundle_meta.get("version", ""),
                "entrypoint": bundle_meta.get("entrypoint", ""),
                "description": bundle_meta.get("description", ""),
                "manifest_path": rel,
            })
        except Exception:
            continue

    return {"bundles": bundles}


# ---------------------------------------------------------------------------
# Dispatcher
# ---------------------------------------------------------------------------

_ACTION_MAP = {
    "create": _create,
    "verify": _verify,
    "inspect": _inspect,
    "list": _list,
}


async def execute(
    action: str, project_path: str, params: Optional[Dict[str, Any]] = None
) -> Dict[str, Any]:
    """Execute a bundler action.

    Args:
        action: One of ACTIONS
        project_path: Path to project root
        params: Action-specific parameters

    Returns:
        Action result dict
    """
    params = params or {}

    if action not in ACTIONS:
        return {"error": f"Unknown action: {action}", "valid_actions": ACTIONS}

    pp = Path(project_path).resolve()
    if not pp.is_dir():
        return {"error": f"Project path does not exist: {project_path}"}

    handler = _ACTION_MAP[action]
    return await handler(pp, params)


# ---------------------------------------------------------------------------
# CLI entry point
# ---------------------------------------------------------------------------

if __name__ == "__main__":
    import argparse
    import sys

    parser = argparse.ArgumentParser(description="Bundler Tool")
    parser.add_argument("--params", required=True, help="Parameters as JSON")
    parser.add_argument("--project-path", required=True, help="Project path")

    args = parser.parse_args()

    try:
        params = json.loads(args.params)
        action = params.pop("action", None)
        if not action:
            print(json.dumps({"success": False, "error": "action required in params"}))
            sys.exit(1)
    except json.JSONDecodeError as e:
        print(json.dumps({"success": False, "error": f"Invalid params JSON: {e}"}))
        sys.exit(1)

    try:
        result = asyncio.run(execute(action, args.project_path, params))
        if "error" in result:
            result["success"] = False
        elif "success" not in result:
            result["success"] = True
        print(json.dumps(result, indent=2), flush=True)
        sys.exit(0 if result.get("success") else 1)
    except Exception as e:
        print(json.dumps({"success": False, "error": str(e)}), flush=True)
        sys.exit(1)
