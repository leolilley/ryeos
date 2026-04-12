"""rye remote <action> — Remote node operations.

Subcommands:
  push / pull / status / execute   CAS sync and remote execution
  publish / search                 Registry operations
  bundle build / push / verify     Bundle operations
"""

from rye_cli.output import print_result


def register(subparsers):
    p = subparsers.add_parser("remote", help="Remote node operations")
    sub = p.add_subparsers(dest="remote_action", required=True)

    # ── CAS sync ──────────────────────────────────────────────────────
    push_p = sub.add_parser("push", help="Sync CAS objects to remote")
    push_p.add_argument("--remote", default="default", help="Remote name")
    push_p.set_defaults(handler=_handle_remote, action="push")

    pull_p = sub.add_parser("pull", help="Pull CAS objects from remote")
    pull_p.add_argument("--remote", default="default", help="Remote name")
    pull_p.set_defaults(handler=_handle_remote, action="pull")

    status_p = sub.add_parser("status", help="Show remote sync status")
    status_p.add_argument("--remote", default="default", help="Remote name")
    status_p.set_defaults(handler=_handle_remote, action="status")

    # ── Execution ─────────────────────────────────────────────────────
    exec_p = sub.add_parser("execute", help="Execute on remote node")
    exec_p.add_argument("item_type", choices=["directive", "tool"])
    exec_p.add_argument("item_id", help="Item ID to execute")
    exec_p.add_argument("--remote", default="default", help="Remote name")
    exec_p.add_argument("--params", default="{}", help="JSON parameters")
    exec_p.set_defaults(handler=_handle_execute)

    # ── Registry ──────────────────────────────────────────────────────
    reg_p = sub.add_parser("registry", help="Registry operations")
    reg_sub = reg_p.add_subparsers(dest="reg_action", required=True)

    pub_p = reg_sub.add_parser("publish", help="Publish item to registry")
    pub_p.add_argument("item_type", choices=["directive", "tool", "knowledge"])
    pub_p.add_argument("item_id", help="Namespaced item ID")
    pub_p.add_argument("--version", help="Version override")
    pub_p.add_argument("--remote", default="registry", help="Remote name")
    pub_p.set_defaults(handler=_handle_publish)

    search_p = reg_sub.add_parser("search", help="Search the registry")
    search_p.add_argument("query", help="Search query")
    search_p.add_argument("--item-type", choices=["directive", "tool", "knowledge", "bundle"])
    search_p.add_argument("--limit", type=int, default=20, help="Max results")
    search_p.add_argument("--remote", default="registry", help="Remote name")
    search_p.set_defaults(handler=_handle_search)

    # ── Bundle operations ─────────────────────────────────────────────
    bundle_p = sub.add_parser("bundle", help="Bundle operations")
    bundle_sub = bundle_p.add_subparsers(dest="bundle_action", required=True)

    bb = bundle_sub.add_parser("build", help="Build bundle manifest")
    bb.add_argument("package_path", help="Path to package containing .ai/")
    bb.add_argument("--bundle-id", help="Override bundle ID")
    bb.add_argument("--version", help="Bundle version")
    bb.add_argument("--description", default="", help="Bundle description")
    bb.set_defaults(handler=_handle_bundle_build)

    bp = bundle_sub.add_parser("push", help="Push bundle to remote")
    bp.add_argument("bundle_id", help="Bundle identifier")
    bp.add_argument("--version", help="Version override")
    bp.add_argument("--remote", default="registry", help="Remote name")
    bp.set_defaults(handler=_handle_bundle_push)

    bpull = bundle_sub.add_parser("pull", help="Pull bundle from remote")
    bpull.add_argument("bundle_id", help="Bundle identifier")
    bpull.add_argument("--version", help="Specific version to pull")
    bpull.add_argument("--remote", default="registry", help="Remote name")
    bpull.set_defaults(handler=_handle_bundle_pull)

    bv = bundle_sub.add_parser("verify", help="Verify bundle signatures")
    bv.add_argument("bundle_id", help="Bundle identifier")
    bv.add_argument("--package-path", help="Package path (default: project path)")
    bv.set_defaults(handler=_handle_bundle_verify)

    # ── Identity ──────────────────────────────────────────────────────
    whoami_p = sub.add_parser("whoami", help="Show current identity")
    whoami_p.set_defaults(handler=_handle_whoami)


# ── Helpers ───────────────────────────────────────────────────────────────


def _remote_execute(project_path: str, params: dict) -> dict:
    from rye_cli.output import daemon_execute
    return daemon_execute("tool:rye/core/remote/remote", project_path=project_path, parameters=params)


def _bundler_execute(project_path: str, params: dict) -> dict:
    from rye_cli.output import daemon_execute
    return daemon_execute("tool:rye/core/bundler/bundler", project_path=project_path, parameters=params)


# ── Handlers ──────────────────────────────────────────────────────────────


def _handle_remote(args, project_path: str):
    params = {"action": args.action, "remote": args.remote}
    result = _remote_execute(project_path, params)
    print_result(result)


def _handle_execute(args, project_path: str):
    import json
    params = {
        "action": "execute",
        "remote": args.remote,
        "item_type": args.item_type,
        "item_id": args.item_id,
        "params": json.loads(args.params),
    }
    result = _remote_execute(project_path, params)
    print_result(result)


def _handle_publish(args, project_path: str):
    params = {
        "action": "publish",
        "item_type": args.item_type,
        "item_id": args.item_id,
        "remote": args.remote,
    }
    if args.version:
        params["version"] = args.version
    result = _remote_execute(project_path, params)
    print_result(result)


def _handle_search(args, project_path: str):
    params = {
        "action": "registry_search",
        "query": args.query,
        "limit": args.limit,
        "remote": args.remote,
    }
    if args.item_type:
        params["item_type"] = args.item_type
    result = _remote_execute(project_path, params)
    print_result(result)


def _handle_bundle_build(args, project_path: str):
    from pathlib import Path
    params = {
        "action": "create-package",
        "package_path": str(Path(args.package_path).resolve()),
    }
    if args.bundle_id:
        params["bundle_id"] = args.bundle_id
    if args.version:
        params["version"] = args.version
    if args.description:
        params["description"] = args.description
    result = _bundler_execute(project_path, params)
    print_result(result)


def _handle_bundle_push(args, project_path: str):
    params = {
        "action": "push_bundle",
        "bundle_id": args.bundle_id,
        "remote": args.remote,
    }
    if args.version:
        params["version"] = args.version
    result = _remote_execute(project_path, params)
    print_result(result)


def _handle_bundle_pull(args, project_path: str):
    params = {
        "action": "pull_bundle",
        "bundle_id": args.bundle_id,
        "remote": args.remote,
    }
    if args.version:
        params["version"] = args.version
    result = _remote_execute(project_path, params)
    print_result(result)


def _handle_bundle_verify(args, project_path: str):
    from pathlib import Path
    effective_path = project_path
    if args.package_path:
        effective_path = str(Path(args.package_path).resolve())
    params = {"action": "verify", "bundle_id": args.bundle_id}
    result = _bundler_execute(effective_path, params)
    print_result(result)
    data = result.get("data", result)
    if data.get("status") != "verified":
        import sys
        sys.exit(1)


def _handle_whoami(args, project_path: str):
    from rye.utils.path_utils import get_signing_key_dir
    from rye.primitives.signing import load_keypair, compute_key_fingerprint
    from pathlib import Path
    try:
        _, pub = load_keypair(Path(get_signing_key_dir()))
        fp = compute_key_fingerprint(pub)
        print_result({"status": "success", "data": {"principal_id": f"fp:{fp}"}})
    except FileNotFoundError:
        print_result({"status": "error", "error": "No signing key. Run: rye execute tool rye/core/keys action=generate"})
