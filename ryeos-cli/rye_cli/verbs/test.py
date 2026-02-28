"""rye test [tool_id] [--spec path] [--include-tags a,b] [--exclude-tags c,d] [--validate-only]

Runs .test.yaml specs via rye/dev/test-runner.
"""

from rye_cli.output import run_async, print_result


def register(subparsers):
    p = subparsers.add_parser("test", help="Run tool test specs")
    p.add_argument("tool_id", nargs="?", help="Tool ID to test (discovers all if omitted)")
    p.add_argument("--spec", help="Path to a specific .test.yaml file")
    p.add_argument("--include-tags", dest="include_tags",
                   help="Comma-separated tags to include")
    p.add_argument("--exclude-tags", dest="exclude_tags",
                   help="Comma-separated tags to exclude")
    p.add_argument("--validate-only", action="store_true",
                   help="Validate specs without executing tools")
    p.set_defaults(handler=handle)


def handle(args, project_path: str):
    from rye.tools.execute import ExecuteTool
    from rye.utils.resolvers import get_user_space

    params = {}
    if args.tool_id:
        params["tool"] = args.tool_id
    if args.spec:
        params["spec"] = args.spec
    if args.include_tags:
        params["include_tags"] = args.include_tags
    if args.exclude_tags:
        params["exclude_tags"] = args.exclude_tags
    if args.validate_only:
        params["validate_only"] = True

    tool = ExecuteTool(str(get_user_space()))
    result = run_async(tool.handle(
        item_type="tool",
        item_id="rye/dev/test-runner",
        project_path=project_path,
        parameters=params,
    ))
    print_result(result)
