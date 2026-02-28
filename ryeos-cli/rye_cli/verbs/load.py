"""rye load <item_type> <item_id> [--source project] [--destination project|user]"""

from rye_cli.output import run_async, print_result


def register(subparsers):
    p = subparsers.add_parser("load", help="Load and inspect an item")
    p.add_argument("item_type", choices=["directive", "tool", "knowledge"],
                   help="Item type")
    p.add_argument("item_id", help="Item ID (slash-separated path)")
    p.add_argument("--source", choices=["project", "user", "system"],
                   help="Restrict lookup to a specific space")
    p.add_argument("--destination", choices=["project", "user"],
                   help="Copy item to this space after loading")
    p.set_defaults(handler=handle)


def handle(args, project_path: str):
    from rye.tools.load import LoadTool
    from rye.utils.resolvers import get_user_space

    kwargs = {
        "item_type": args.item_type,
        "item_id": args.item_id,
        "project_path": project_path,
    }
    if args.source:
        kwargs["source"] = args.source
    if args.destination:
        kwargs["destination"] = args.destination

    tool = LoadTool(str(get_user_space()))
    result = run_async(tool.handle(**kwargs))
    print_result(result)
