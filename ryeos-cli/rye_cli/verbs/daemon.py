"""rye daemon init|serve|identity — Daemon bootstrap and management."""

import json
import os
import sys
import urllib.error
import urllib.request

from rye_cli.output import daemon_url, die


def register(subparsers):
    p = subparsers.add_parser("daemon", help="Daemon management")
    sub = p.add_subparsers(dest="daemon_action", required=True)

    # init
    init_p = sub.add_parser("init", help="Bootstrap daemon state directory")
    init_p.add_argument(
        "--force", action="store_true", help="Overwrite existing key and config"
    )
    init_p.add_argument(
        "--dev", action="store_true", help="Use XDG defaults for local dev"
    )
    init_p.set_defaults(handler=_handle_init)

    # serve
    serve_p = sub.add_parser("serve", help="Start the daemon")
    serve_p.add_argument(
        "--init-if-missing",
        action="store_true",
        help="Run init with defaults before starting",
    )
    serve_p.add_argument(
        "--bind", default="127.0.0.1:7400", help="HTTP bind address"
    )
    serve_p.set_defaults(handler=_handle_serve)

    # identity export
    id_p = sub.add_parser("identity", help="Identity operations")
    id_sub = id_p.add_subparsers(dest="id_action", required=True)
    export_p = id_sub.add_parser("export", help="Print public identity document")
    export_p.set_defaults(handler=_handle_identity_export)


def _handle_init(args, project_path: str):
    """Run ryeosd bootstrap init."""
    cmd = ["ryeosd", "--init-if-missing"]
    if hasattr(args, "bind"):
        cmd.extend(["--bind", args.bind])
    print(
        "Run: ryeosd --init-if-missing",
        file=sys.stderr,
    )
    print(
        "The daemon will initialize on first start with --init-if-missing",
        file=sys.stderr,
    )


def _handle_serve(args, project_path: str):
    """Start the daemon."""
    cmd = ["ryeosd", "--bind", args.bind]
    if args.init_if_missing:
        cmd.append("--init-if-missing")
    os.execvp("ryeosd", cmd)


def _handle_identity_export(args, project_path: str):
    """Export the stable public identity document."""
    url = f"{daemon_url()}/public-key"
    try:
        with urllib.request.urlopen(url) as resp:
            doc = json.loads(resp.read())
            print(json.dumps(doc, indent=2))
    except urllib.error.URLError as e:
        die(f"Cannot connect to daemon: {e.reason}")
