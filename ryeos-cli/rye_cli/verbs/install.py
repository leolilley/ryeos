"""rye install <bundle_id>[@version] — Install a bundle from the registry.

Flow: search_bundle → pull_bundle via CAS sync → materialize into
~/.ai/bundles/{bundle_id}/ → verify signatures.
"""

from rye_cli.output import run_async, print_result


def register(subparsers):
    p = subparsers.add_parser(
        "install",
        help="Install a bundle from the registry",
    )
    p.add_argument(
        "bundle_spec",
        help="Bundle ID: my-bundle, my-bundle@1.0.0, namespace/my-bundle, or namespace/my-bundle@1.0.0",
    )
    p.add_argument(
        "--space",
        default="user",
        choices=["user", "project"],
        help="Install target: user (~/.ai/) or project (.ai/) (default: user)",
    )
    p.set_defaults(handler=_handle_install)


def _handle_install(args, project_path: str):
    """Install a bundle by pulling from registry and materializing locally."""
    from rye.actions.execute import ExecuteTool
    from rye.utils.resolvers import get_user_space

    # Parse bundle_spec: "bundle_id", "bundle_id@version",
    # "namespace/bundle_id", or "namespace/bundle_id@version"
    spec = args.bundle_spec
    if "@" in spec:
        bundle_part, version = spec.rsplit("@", 1)
    else:
        bundle_part = spec
        version = None

    # Check if namespace is included (contains /)
    if "/" in bundle_part:
        namespace, bundle_id = bundle_part.split("/", 1)
        # Pass full namespace/bundle_id so the registry API resolves it
        bundle_id = f"{namespace}/{bundle_id}"
    else:
        bundle_id = bundle_part

    # Step 1: Pull bundle from registry
    tool = ExecuteTool(str(get_user_space()))
    params = {
        "action": "pull_bundle",
        "bundle_id": bundle_id,
    }
    if version:
        params["version"] = version

    # Use project_path for pull target based on --space
    if args.space == "user":
        from rye.utils.path_utils import get_user_space as get_user_root
        target_path = str(get_user_root())
    else:
        target_path = project_path

    params["project_path"] = target_path

    result = run_async(tool.handle(
        item_type="tool",
        item_id="rye/core/registry/registry",
        project_path=target_path,
        parameters=params,
    ))

    if result.get("error"):
        print_result(result)
        return

    # Step 2: Verify bundle signatures
    verify_params = {
        "action": "verify",
        "bundle_id": bundle_id,
    }
    verify_result = run_async(tool.handle(
        item_type="tool",
        item_id="rye/core/bundler/bundler",
        project_path=target_path,
        parameters=verify_params,
    ))

    # Combine results
    verify_ok = (
        verify_result.get("status") != "failed"
        and not verify_result.get("error")
    )
    output = {
        "status": "installed" if verify_ok else "installed_with_warnings",
        "bundle_id": bundle_id,
        "version": result.get("version", version or "latest"),
        "space": args.space,
        "target_path": target_path,
        "file_count": result.get("file_count", 0),
        "verification": verify_result.get("status", "skipped"),
    }

    if verify_result.get("error"):
        output["verification_error"] = verify_result["error"]

    print_result(output)
