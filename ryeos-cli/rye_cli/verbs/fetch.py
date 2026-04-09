"""rye fetch <item_id> | rye fetch --query <q> --scope <s>

item_id supports canonical refs (e.g. 'tool:rye/bash/bash', 'knowledge:my/entry').
Bare IDs auto-detect across directive/tool/knowledge.
"""

from rye_cli.output import run_async, print_result


def register(subparsers):
    p = subparsers.add_parser("fetch", help="Resolve items by ID or discover by query")
    p.add_argument("item_id", nargs="?", default=None,
                   help="Item ID or canonical ref (tool:id, directive:id, knowledge:id)")
    p.add_argument("--query", help="Search query (triggers query mode)")
    p.add_argument("--scope", help="Item type scope for query mode (e.g., tool, directive, tool.rye.core.*)")
    p.add_argument("--source", choices=["project", "user", "system", "local", "registry", "all"],
                   help="Restrict lookup to a specific space")
    p.add_argument("--destination", choices=["project", "user"],
                   help="Copy item to this space after resolving (ID mode only)")
    p.add_argument("--version", help="Version to pull (registry only)")
    p.add_argument("--limit", type=int, default=10, help="Max results for query mode (default: 10)")
    p.set_defaults(handler=handle)


def handle(args, project_path: str):
    from rye.actions.fetch import FetchTool
    from rye.utils.resolvers import get_user_space

    kwargs = {"project_path": project_path}

    if args.query:
        # Query mode
        kwargs["query"] = args.query
        if args.scope:
            kwargs["scope"] = args.scope
        if args.source:
            kwargs["source"] = args.source
        kwargs["limit"] = args.limit
    elif args.item_id:
        # ID mode
        kwargs["item_id"] = args.item_id
        if args.source:
            kwargs["source"] = args.source
        if args.destination:
            kwargs["destination"] = args.destination
        if args.version:
            kwargs["version"] = args.version
    else:
        print("Error: provide item_id or --query")
        return

    tool = FetchTool(str(get_user_space()))
    result = run_async(tool.handle(**kwargs))
    print_result(result)
