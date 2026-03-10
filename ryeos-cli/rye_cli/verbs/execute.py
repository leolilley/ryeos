"""rye execute <item_type> <item_id> [--dry-run]

Parameters are read as JSON from stdin."""

import sys

from rye_cli.output import run_async, print_result, parse_params


def register(subparsers):
    p = subparsers.add_parser("execute", help="Execute a tool, directive, or knowledge item")
    p.add_argument("item_type", choices=["directive", "tool", "knowledge"],
                   help="Item type")
    p.add_argument("item_id", help="Item ID (slash-separated path)")
    p.add_argument("--dry-run", action="store_true",
                   help="Validate without executing")
    p.set_defaults(handler=handle)


def handle(args, project_path: str):
    from rye.tools.execute import ExecuteTool
    from rye.utils.resolvers import get_user_space

    raw = sys.stdin.read().strip() if not sys.stdin.isatty() else "{}"
    params = parse_params(raw)

    tool = ExecuteTool(str(get_user_space()))
    result = run_async(tool.handle(
        item_type=args.item_type,
        item_id=args.item_id,
        project_path=project_path,
        parameters=params,
        dry_run=args.dry_run,
    ))
    print_result(result)
