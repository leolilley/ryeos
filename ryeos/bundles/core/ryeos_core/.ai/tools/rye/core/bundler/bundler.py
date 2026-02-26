# rye:signed:2026-02-26T04:41:19Z:56e383536dcbfb68671c5dbd7dd4aa8eb84a08151e72563a016f82c59addc7de:haSaortdVw7Jg8H9HU_RJIAK_XlrDH9MH4vU5mNbKTH9YvOIiNHcJc_916jqZBf02m7aRY7J4fyinwGLvskrAQ==:9fbfabe975fa5a7f

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
__executor_id__ = "rye/core/runtimes/python/script"
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

ACTIONS = ["create", "create-package", "verify", "inspect", "list"]

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "action": {
            "type": "string",
            "enum": ACTIONS,
            "description": "Bundle operation to perform: create (project), create-package (package), verify, inspect, list",
        },
        "bundle_id": {
            "type": "string",
            "description": "Bundle identifier, e.g., apps/task-manager or rye-mcp",
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
        "package_path": {
            "type": "string",
            "description": "For create-package: path to package root containing .ai/ directory",
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

# Additional .ai/ subdirectories to include in bundles
_EXTRA_DIRS = ["trusted_keys"]


# ---------------------------------------------------------------------------
# Bundle Discovery & Validation
# ---------------------------------------------------------------------------


def validate_bundle_manifest(
    manifest_path: Path,
    project_path: Optional[Path] = None,
    require_all_hashes: bool = True,
) -> Dict[str, Any]:
    """Validate a bundle manifest and optionally its files.

    Performs manifest-level validation:
    1. Verify manifest's own inline signature (Ed25519)
    2. Check bundle metadata structure
    3. If require_all_hashes: verify every file hash matches

    Args:
        manifest_path: Path to manifest.yaml
        project_path: Project root for resolving relative file paths
        require_all_hashes: If True, verify all file hashes (slow for large bundles)

    Returns:
        Dict with keys:
            - valid: bool - overall validity
            - manifest_valid: bool - signature check passed
            - bundle_id: str - extracted bundle identifier
            - version: str - extracted bundle version
            - files_checked: int - number of files in manifest
            - files_ok: int - files with matching hashes
            - files_missing: List[str] - files not found on disk
            - files_tampered: List[str] - files with hash mismatch
            - error: str - error message if validation failed
    """
    from rye.utils.integrity import verify_item, IntegrityError
    from rye.constants import ItemType

    if not manifest_path.exists():
        return {
            "valid": False,
            "manifest_valid": False,
            "error": f"Manifest not found: {manifest_path}",
        }

    result = {
        "valid": False,
        "manifest_valid": False,
        "bundle_id": "",
        "version": "",
        "files_checked": 0,
        "files_ok": 0,
        "files_missing": [],
        "files_tampered": [],
    }

    # 1. Verify manifest's own signature
    try:
        verify_item(manifest_path, ItemType.TOOL, project_path=project_path)
        result["manifest_valid"] = True
    except IntegrityError as e:
        result["error"] = f"Manifest signature invalid: {e}"
        return result

    # 2. Parse manifest content
    try:
        data = _parse_manifest(manifest_path)
        bundle_meta = data.get("bundle", {})
        result["bundle_id"] = bundle_meta.get("id", "")
        result["version"] = bundle_meta.get("version", "")
        file_entries = data.get("files", {})
        result["files_checked"] = len(file_entries)
    except Exception as e:
        result["error"] = f"Failed to parse manifest: {e}"
        return result

    if not require_all_hashes:
        # Just metadata validation
        result["valid"] = True
        return result

    # 3. Verify file hashes
    base_path = (
        project_path if project_path else manifest_path.parent.parent.parent.parent
    )

    for rel_path, meta in file_entries.items():
        file_path = base_path / rel_path
        if not file_path.exists():
            result["files_missing"].append(rel_path)
            continue

        actual_hash = _sha256_file(file_path)
        if actual_hash != meta.get("sha256"):
            result["files_tampered"].append(rel_path)
            continue

        # If file claims inline signature, verify it too
        if meta.get("inline_signed"):
            item_type = _classify_file(rel_path)
            if item_type in ("directive", "tool", "knowledge"):
                try:
                    verify_item(file_path, item_type, project_path=base_path)
                except IntegrityError:
                    result["files_tampered"].append(rel_path)
                    continue

        result["files_ok"] += 1

    # Valid if manifest signed and all files match (or no file checking requested)
    result["valid"] = (
        result["manifest_valid"]
        and len(result["files_missing"]) == 0
        and len(result["files_tampered"]) == 0
    )

    return result


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
    """Classify a file into directive/tool/knowledge/trusted_key/asset by its relative path."""
    for item_type, dir_name in _TYPE_DIRS.items():
        if rel_path.startswith(f"{AI_DIR}/{dir_name}/"):
            return item_type
    for dir_name in _EXTRA_DIRS:
        if rel_path.startswith(f"{AI_DIR}/{dir_name}/"):
            return dir_name
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
    from rye.utils.path_utils import get_user_space, get_system_spaces

    config_name = "rye/core/bundler/collect.yaml"
    search_order = [
        project_path / AI_DIR / "tools" / config_name,
        get_user_space() / AI_DIR / "tools" / config_name,
    ]
    for bundle in get_system_spaces():
        search_order.append(bundle.root_path / AI_DIR / "tools" / config_name)

    for path in search_order:
        if path.exists():
            try:
                data = yaml.safe_load(path.read_text(encoding="utf-8"))
                if isinstance(data, dict):
                    return data
            except Exception:
                continue

    return {}


def _collect_bundle_files(project_path: Path, bundle_id: str) -> List[Dict[str, Any]]:
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
            files.append(
                {
                    "path": rel,
                    "sha256": _sha256_file(file_path),
                    "inline_signed": _has_inline_signature(file_path),
                }
            )

    # Extra directories (trusted_keys, etc.)
    for dir_name in _EXTRA_DIRS:
        extra_dir = project_path / AI_DIR / dir_name
        if not extra_dir.is_dir():
            continue

        for file_path in sorted(extra_dir.rglob("*")):
            if not file_path.is_file():
                continue
            if any(d in file_path.parts for d in exclude_dirs):
                continue
            rel = str(file_path.relative_to(project_path))
            files.append(
                {
                    "path": rel,
                    "sha256": _sha256_file(file_path),
                    "inline_signed": _has_inline_signature(file_path),
                }
            )

    # Plans and lockfiles (use slug: bundle_id with / -> _)
    bundle_slug = bundle_id.replace("/", "_")

    plans_dir = project_path / AI_DIR / "plans" / bundle_slug
    if plans_dir.is_dir():
        for file_path in sorted(plans_dir.rglob("*")):
            if not file_path.is_file():
                continue
            rel = str(file_path.relative_to(project_path))
            files.append(
                {
                    "path": rel,
                    "sha256": _sha256_file(file_path),
                    "inline_signed": _has_inline_signature(file_path),
                }
            )

    lockfiles_dir = project_path / AI_DIR / "lockfiles"
    if lockfiles_dir.is_dir():
        prefix = f"{bundle_slug}_"
        for file_path in sorted(lockfiles_dir.iterdir()):
            if file_path.is_file() and file_path.name.startswith(prefix):
                rel = str(file_path.relative_to(project_path))
                files.append(
                    {
                        "path": rel,
                        "sha256": _sha256_file(file_path),
                        "inline_signed": _has_inline_signature(file_path),
                    }
                )

    return files


def _files_by_type(files: List[Dict[str, Any]]) -> Dict[str, int]:
    """Count files by classified type."""
    counts: Dict[str, int] = {}
    for f in files:
        t = _classify_file(f["path"])
        counts[t] = counts.get(t, 0) + 1
    return counts


def _sign_manifest(content: str) -> str:
    """Sign manifest YAML content and prepend signature line.

    Uses the same Ed25519 signing infrastructure as all other rye items.
    """
    from rye.utils.metadata_manager import compute_content_hash, generate_timestamp
    from lillux.primitives.signing import (
        ensure_keypair,
        sign_hash,
        compute_key_fingerprint,
    )
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


async def _create(project_path: Path, params: Dict[str, Any]) -> Dict[str, Any]:
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
            "searched": [f"{AI_DIR}/{d}/{bundle_id}/" for d in _TYPE_DIRS.values()],
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


async def _create_package(project_path: Path, params: Dict[str, Any]) -> Dict[str, Any]:
    """Create a signed bundle manifest for a package (not a project).

    Package bundles live at .ai/bundles/{bundle_id}/manifest.yaml within
    the package's .ai directory, not in a project. This is used when
    publishing bundles via Python packages with rye.bundles entry points.

    Args:
        project_path: Not used, kept for API consistency
        params: Must include 'package_path' and 'bundle_id'

    Returns:
        Same format as _create
    """
    package_path_str = params.get("package_path")
    if not package_path_str:
        return {"error": "package_path is required for create-package"}

    package_path = Path(package_path_str).resolve()
    if not package_path.is_dir():
        return {"error": f"Package path does not exist: {package_path}"}

    # Check for .ai directory in package
    ai_dir = package_path / AI_DIR
    if not ai_dir.exists():
        return {
            "error": f"No .ai directory found in package: {package_path}",
            "hint": "Package must contain .ai/ with directives/, tools/, or knowledge/",
        }

    bundle_id = params.get("bundle_id")
    if not bundle_id:
        # Try to infer from package name or directory
        bundle_id = package_path.name

    version = params.get("version", "0.1.0")
    entrypoint = params.get("entrypoint", "")
    description = params.get("description", "")

    # Collect all files from the package's .ai directory
    files = _collect_package_files(package_path, bundle_id)
    if not files:
        return {
            "error": f"No files found for bundle '{bundle_id}' in package",
            "searched_paths": [str(ai_dir / d) for d in _TYPE_DIRS.values()],
        }

    manifest = {
        "bundle": {
            "id": bundle_id,
            "version": version,
            "entrypoint": entrypoint,
            "description": description,
            "created_at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
            "type": "package",  # Distinguish from project bundles
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

    # Package bundles go in .ai/bundles/{bundle_id}/
    manifest_dir = ai_dir / "bundles" / bundle_id
    manifest_dir.mkdir(parents=True, exist_ok=True)
    manifest_path = manifest_dir / "manifest.yaml"
    manifest_path.write_text(signed_yaml, encoding="utf-8")

    return {
        "status": "created",
        "bundle_type": "package",
        "manifest_path": str(manifest_path),
        "file_count": len(files),
        "files_by_type": _files_by_type(files),
    }


def _collect_package_files(package_path: Path, bundle_id: str) -> List[Dict[str, Any]]:
    """Collect all files from a package's .ai directory.

    Unlike project bundles which filter by bundle_id subdirectory,
    package bundles include ALL files under .ai/ since the entire
    package is the bundle.
    """
    collect_config = _load_collect_config(package_path)
    exclude_dirs = set(collect_config.get("exclude_dirs", []))

    files: List[Dict[str, Any]] = []
    ai_dir = package_path / AI_DIR

    if not ai_dir.exists():
        return files

    # Walk all item type directories
    for item_type, dir_name in _TYPE_DIRS.items():
        type_dir = ai_dir / dir_name
        if not type_dir.is_dir():
            continue

        for file_path in sorted(type_dir.rglob("*")):
            if not file_path.is_file():
                continue
            if any(d in file_path.parts for d in exclude_dirs):
                continue

            # Compute path relative to package root (not type dir)
            rel = str(file_path.relative_to(package_path))
            files.append(
                {
                    "path": rel,
                    "sha256": _sha256_file(file_path),
                    "inline_signed": _has_inline_signature(file_path),
                    "item_type": item_type,
                }
            )

    # Walk extra directories (trusted_keys, etc.)
    for dir_name in _EXTRA_DIRS:
        extra_dir = ai_dir / dir_name
        if not extra_dir.is_dir():
            continue

        for file_path in sorted(extra_dir.rglob("*")):
            if not file_path.is_file():
                continue

            rel = str(file_path.relative_to(package_path))
            files.append(
                {
                    "path": rel,
                    "sha256": _sha256_file(file_path),
                    "inline_signed": _has_inline_signature(file_path),
                    "item_type": "asset",
                }
            )

    return files


async def _verify(project_path: Path, params: Dict[str, Any]) -> Dict[str, Any]:
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
        "status": "verified"
        if manifest_valid and not files_missing and not files_tampered
        else "failed",
        "manifest_valid": manifest_valid,
        "files_checked": len(file_entries),
        "files_ok": files_ok,
        "files_missing": files_missing,
        "files_tampered": files_tampered,
    }
    if manifest_error:
        result["manifest_error"] = manifest_error
    return result


async def _inspect(project_path: Path, params: Dict[str, Any]) -> Dict[str, Any]:
    """Inspect a bundle manifest without verification."""
    bundle_id = params.get("bundle_id")
    if not bundle_id:
        return {"error": "bundle_id is required for inspect"}

    manifest_path = project_path / AI_DIR / "bundles" / bundle_id / "manifest.yaml"
    if not manifest_path.exists():
        return {"error": f"Manifest not found: {manifest_path}"}

    data = _parse_manifest(manifest_path)
    file_entries = data.get("files", {})

    files_list = [{"path": path, **meta} for path, meta in file_entries.items()]

    classified = _files_by_type([{"path": f["path"]} for f in files_list])

    return {
        "bundle": data.get("bundle", {}),
        "files": files_list,
        "file_count": len(files_list),
        "files_by_type": classified,
    }


async def _list(project_path: Path, params: Dict[str, Any]) -> Dict[str, Any]:
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
            bundles.append(
                {
                    "bundle_id": bundle_meta.get("id", ""),
                    "version": bundle_meta.get("version", ""),
                    "entrypoint": bundle_meta.get("entrypoint", ""),
                    "description": bundle_meta.get("description", ""),
                    "manifest_path": rel,
                }
            )
        except Exception:
            continue

    return {"bundles": bundles}


# ---------------------------------------------------------------------------
# Dispatcher
# ---------------------------------------------------------------------------

_ACTION_MAP = {
    "create": _create,
    "create-package": _create_package,
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
