"""rye sign <item_id> [--source project|user]

item_id uses canonical ref prefix (e.g. 'tool:rye/bash/bash', 'directive:*').
"""

from rye_cli.output import run_async, print_result


def register(subparsers):
    p = subparsers.add_parser("sign", help="Sign an item with Ed25519")
    p.add_argument("item_id", help="Canonical ref (tool:id, directive:id) or glob pattern (directive:*)")
    p.add_argument("--source", default="project", choices=["project", "user"],
                   help="Space where the item lives (default: project)")
    p.set_defaults(handler=handle)


def handle(args, project_path: str):
    from rye.actions.sign import SignTool
    from rye.utils.resolvers import get_user_space

    tool = SignTool(str(get_user_space()))
    result = run_async(tool.handle(
        item_id=args.item_id,
        project_path=project_path,
        source=args.source,
    ))
    print_result(result)
