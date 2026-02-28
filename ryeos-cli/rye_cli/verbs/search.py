"""rye search <scope> <query> [--space all] [--limit 10]"""

from rye_cli.output import run_async, print_result


def register(subparsers):
    p = subparsers.add_parser("search", help="Search for items by keyword")
    p.add_argument("scope", help="Item type scope (e.g., directive, tool, knowledge, tool.rye.core.*)")
    p.add_argument("query", help="Search query (use '*' to list all)")
    p.add_argument("--space", default="all", choices=["project", "user", "system", "all"],
                   help="Space to search (default: all)")
    p.add_argument("--limit", type=int, default=10, help="Max results (default: 10)")
    p.set_defaults(handler=handle)


def handle(args, project_path: str):
    from rye.tools.search import SearchTool
    from rye.utils.resolvers import get_user_space

    tool = SearchTool(str(get_user_space()))
    result = run_async(tool.handle(
        item_type=args.scope,
        query=args.query,
        project_path=project_path,
        source=args.space,
        limit=args.limit,
    ))
    print_result(result)
