"""rye uninstall <bundle_id> — Remove an installed bundle.

Removes the materialized bundle directory and any files it installed.
"""

import json
import shutil
from pathlib import Path

from rye_cli.output import print_result


def register(subparsers):
    p = subparsers.add_parser(
        "uninstall",
        help="Uninstall a bundle",
    )
    p.add_argument(
        "bundle_id",
        help="Bundle ID to uninstall (e.g., my-bundle or namespace/my-bundle)",
    )
    p.add_argument(
        "--space",
        default="user",
        choices=["user", "project"],
        help="Space to uninstall from: user (~/.ai/) or project (.ai/) (default: user)",
    )
    p.set_defaults(handler=_handle_uninstall)


def _handle_uninstall(args, project_path: str):
    """Uninstall a bundle by removing its materialized files."""
    from rye.constants import AI_DIR
    from rye.utils.path_utils import get_user_space

    bundle_id = args.bundle_id

    if args.space == "user":
        target_path = Path(get_user_space())
    else:
        target_path = Path(project_path)

    bundle_dir = target_path / AI_DIR / "bundles" / bundle_id
    if not bundle_dir.exists():
        print_result({
            "error": f"Bundle '{bundle_id}' is not installed in {args.space} space",
        })
        return

    files_removed = 0
    lock_path = bundle_dir / "install-receipt.json"
    if lock_path.exists():
        try:
            lock_data = json.loads(lock_path.read_text())
            for rel_path in lock_data.get("files", []):
                file_path = target_path / rel_path
                if file_path.is_file():
                    file_path.unlink()
                    files_removed += 1
                    # Clean up empty parent dirs up to .ai/
                    parent = file_path.parent
                    ai_root = target_path / AI_DIR
                    while parent != ai_root and parent.is_dir() and not any(parent.iterdir()):
                        parent.rmdir()
                        parent = parent.parent
        except (json.JSONDecodeError, KeyError):
            print_result({
                "warning": "Lockfile corrupt; skipping file cleanup",
            })
    else:
        print_result({
            "warning": "No lockfile found; skipping file cleanup",
        })

    # Remove the bundle directory itself
    shutil.rmtree(bundle_dir, ignore_errors=True)

    # Clean up empty bundles/ parent dir
    bundles_parent = bundle_dir.parent
    if bundles_parent.is_dir() and not any(bundles_parent.iterdir()):
        bundles_parent.rmdir()

    print_result({
        "status": "uninstalled",
        "bundle_id": bundle_id,
        "space": args.space,
        "files_removed": files_removed,
    })
