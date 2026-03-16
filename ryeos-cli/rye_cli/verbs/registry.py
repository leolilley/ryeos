"""rye registry <action> — Registry and bundle operations.

Subcommands:
  login / logout / whoami          Auth management
  push / pull / search             Single item operations
  publish / unpublish              Visibility control
  bundle build / push / pull       Bundle operations
"""

from rye_cli.output import run_async, print_result


def register(subparsers):
    p = subparsers.add_parser("registry", help="Rye Registry operations")
    sub = p.add_subparsers(dest="registry_action", required=True)

    # ── Auth ──────────────────────────────────────────────────────────
    login_p = sub.add_parser("login", help="Authenticate with the registry")
    login_p.set_defaults(handler=_handle_auth, action="login")

    logout_p = sub.add_parser("logout", help="Clear auth session")
    logout_p.set_defaults(handler=_handle_auth, action="logout")

    whoami_p = sub.add_parser("whoami", help="Show current user")
    whoami_p.set_defaults(handler=_handle_auth, action="whoami")

    # ── Single item operations ────────────────────────────────────────
    push_p = sub.add_parser("push", help="Push item to registry")
    push_p.add_argument("item_type", choices=["directive", "tool", "knowledge"])
    push_p.add_argument("item_id", help="Namespaced item ID (e.g., leolilley/utils/tool)")
    push_p.add_argument("--version", help="Version override")
    push_p.set_defaults(handler=_handle_push)

    pull_p = sub.add_parser("pull", help="Pull item from registry")
    pull_p.add_argument("item_type", choices=["directive", "tool", "knowledge"])
    pull_p.add_argument("item_id", help="Namespaced item ID")
    pull_p.add_argument("--version", help="Specific version to pull")
    pull_p.add_argument("--space", default="project",
                        choices=["project", "user"], help="Target space (default: project)")
    pull_p.set_defaults(handler=_handle_pull)

    search_p = sub.add_parser("search", help="Search the registry")
    search_p.add_argument("query", help="Search query")
    search_p.add_argument("--item-type", choices=["directive", "tool", "knowledge"],
                          help="Filter by item type")
    search_p.add_argument("--limit", type=int, default=20, help="Max results")
    search_p.set_defaults(handler=_handle_search)

    # ── Bundle operations ─────────────────────────────────────────────
    bundle_p = sub.add_parser("bundle", help="Bundle operations")
    bundle_sub = bundle_p.add_subparsers(dest="bundle_action", required=True)

    # bundle build
    bb = bundle_sub.add_parser("build", help="Build bundle manifest from package")
    bb.add_argument("package_path", help="Path to package containing .ai/")
    bb.add_argument("--bundle-id", help="Override bundle ID (default: inferred from dir)")
    bb.add_argument("--version", help="Bundle version")
    bb.add_argument("--description", default="", help="Bundle description")
    bb.set_defaults(handler=_handle_bundle_build)

    # bundle push
    bp = bundle_sub.add_parser("push", help="Push bundle to registry")
    bp.add_argument("bundle_id", help="Bundle identifier")
    bp.add_argument("--version", help="Version override")
    bp.set_defaults(handler=_handle_bundle_push)

    # bundle pull
    bpull = bundle_sub.add_parser("pull", help="Pull bundle from registry")
    bpull.add_argument("bundle_id", help="Bundle identifier")
    bpull.add_argument("--version", help="Specific version to pull")
    bpull.set_defaults(handler=_handle_bundle_pull)

    # bundle verify
    bv = bundle_sub.add_parser("verify", help="Verify bundle signatures")
    bv.add_argument("bundle_id", help="Bundle identifier")
    bv.add_argument("--package-path", help="Package path (default: project path)")
    bv.set_defaults(handler=_handle_bundle_verify)

    # bundle search
    bs = bundle_sub.add_parser("search", help="Search bundles in registry")
    bs.add_argument("query", help="Search query")
    bs.add_argument("--namespace", help="Filter by namespace")
    bs.add_argument("--limit", type=int, default=20, help="Max results")
    bs.set_defaults(handler=_handle_bundle_search)


# ── Handlers ──────────────────────────────────────────────────────────────


def _registry_execute(project_path: str, params: dict) -> dict:
    """Call the registry tool via ExecuteTool."""
    from rye.tools.execute import ExecuteTool
    from rye.utils.resolvers import get_user_space

    tool = ExecuteTool(str(get_user_space()))
    return run_async(tool.handle(
        item_type="tool",
        item_id="rye/core/registry/registry",
        project_path=project_path,
        parameters=params,
    ))


def _bundler_execute(project_path: str, params: dict) -> dict:
    """Call the bundler tool via ExecuteTool."""
    from rye.tools.execute import ExecuteTool
    from rye.utils.resolvers import get_user_space

    tool = ExecuteTool(str(get_user_space()))
    return run_async(tool.handle(
        item_type="tool",
        item_id="rye/core/bundler/bundler",
        project_path=project_path,
        parameters=params,
    ))


def _handle_auth(args, project_path: str):
    result = _registry_execute(project_path, {"action": args.action})
    print_result(result)


def _handle_push(args, project_path: str):
    params = {
        "action": "push",
        "item_type": args.item_type,
        "item_id": args.item_id,
    }
    if args.version:
        params["version"] = args.version
    result = _registry_execute(project_path, params)
    print_result(result)


def _handle_pull(args, project_path: str):
    params = {
        "action": "pull",
        "item_type": args.item_type,
        "item_id": args.item_id,
        "location": args.space,
    }
    if args.version:
        params["version"] = args.version
    result = _registry_execute(project_path, params)
    print_result(result)


def _handle_search(args, project_path: str):
    params = {
        "action": "search",
        "query": args.query,
        "limit": args.limit,
    }
    if args.item_type:
        params["item_type"] = args.item_type
    result = _registry_execute(project_path, params)
    print_result(result)


def _handle_bundle_build(args, project_path: str):
    from pathlib import Path

    package_path = str(Path(args.package_path).resolve())
    params = {
        "action": "create-package",
        "package_path": package_path,
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
    }
    if args.version:
        params["version"] = args.version
    result = _registry_execute(project_path, params)
    print_result(result)


def _handle_bundle_pull(args, project_path: str):
    params = {
        "action": "pull_bundle",
        "bundle_id": args.bundle_id,
    }
    if args.version:
        params["version"] = args.version
    result = _registry_execute(project_path, params)
    print_result(result)


def _handle_bundle_verify(args, project_path: str):
    from pathlib import Path

    effective_path = project_path
    if args.package_path:
        effective_path = str(Path(args.package_path).resolve())

    params = {
        "action": "verify",
        "bundle_id": args.bundle_id,
    }
    result = _bundler_execute(effective_path, params)
    print_result(result)

    data = result.get("data", result)
    if data.get("status") != "verified":
        import sys
        sys.exit(1)


def _handle_bundle_search(args, project_path: str):
    params = {
        "action": "search_bundle",
        "query": args.query,
        "limit": args.limit,
    }
    if args.namespace:
        params["namespace"] = args.namespace
    result = _registry_execute(project_path, params)
    print_result(result)
