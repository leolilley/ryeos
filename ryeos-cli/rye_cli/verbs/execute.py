"""rye execute <item_id> [--dry-run]

Parameters are read as JSON from stdin."""

import sys

from rye_cli.output import daemon_execute, print_result, parse_params


def register(subparsers):
    p = subparsers.add_parser("execute", help="Execute a tool or directive")
    p.add_argument("item_id", help="Item ID (slash-separated path, or kind:id canonical ref)")
    p.add_argument("--dry-run", action="store_true",
                   help="Validate without executing")
    p.set_defaults(handler=handle)


def handle(args, project_path: str):
    raw = sys.stdin.read().strip() if not sys.stdin.isatty() else "{}"
    params = parse_params(raw)

    if args.dry_run:
        params["dry_run"] = True

    result = daemon_execute(args.item_id, params)
    print_result(result)
