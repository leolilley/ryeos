"""rye test [tool_id] [--spec path] [--include-tags a,b] [--exclude-tags c,d] [--validate-only]

Runs .test.yaml specs via rye/dev/test-runner.
"""

from rye_cli.output import daemon_execute, print_result


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

    result = daemon_execute("tool:rye/dev/test-runner", project_path=project_path, parameters=params)
    print_result(result)
