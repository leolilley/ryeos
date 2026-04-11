"""rye install <bundle_id>[@version] — Install a bundle from the registry.

Flow: search_bundle → pull_bundle via CAS sync → materialize into
~/.ai/bundles/{bundle_id}/ → verify signatures.
"""

from rye_cli.output import daemon_execute, print_result


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
    spec = args.bundle_spec
    if "@" in spec:
        bundle_part, version = spec.rsplit("@", 1)
    else:
        bundle_part = spec
        version = None

    if "/" in bundle_part:
        namespace, bundle_id = bundle_part.split("/", 1)
        bundle_id = f"{namespace}/{bundle_id}"
    else:
        bundle_id = bundle_part

    # Step 1: Pull bundle from registry
    params = {
        "action": "pull_bundle",
        "bundle_id": bundle_id,
        "remote": "registry",
    }
    if version:
        params["version"] = version

    result = daemon_execute("tool:rye/core/remote/remote", params)

    if result.get("error"):
        print_result(result)
        return

    # Step 2: Verify bundle signatures
    verify_params = {
        "action": "verify",
        "bundle_id": bundle_id,
    }
    verify_result = daemon_execute("tool:rye/core/bundler/bundler", verify_params)

    verify_ok = (
        verify_result.get("status") != "failed"
        and not verify_result.get("error")
    )
    output = {
        "status": "installed" if verify_ok else "installed_with_warnings",
        "bundle_id": bundle_id,
        "version": result.get("version", version or "latest"),
        "space": args.space,
        "file_count": result.get("file_count", 0),
        "verification": verify_result.get("status", "skipped"),
    }

    if verify_result.get("error"):
        output["verification_error"] = verify_result["error"]

    print_result(output)
