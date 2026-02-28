"""rye thread <directive_id> [--params '{...}'] [--model sonnet] [--max-spend 1.00] [--async]

Convenience alias for 'rye execute directive' with explicit thread-level flags.
Separates thread flags (--model, --max-spend, --async, --max-turns) from
directive inputs (--params).
"""

import sys

from rye_cli.output import run_async, print_result, parse_params


def register(subparsers):
    p = subparsers.add_parser("thread", help="Spawn a directive thread")
    p.add_argument("directive_id", help="Directive ID (slash-separated path)")
    p.add_argument("--params", default="{}", dest="params_json",
                   help="Directive input parameters as JSON string")

    # Thread-level flags (known fixed set)
    p.add_argument("--model", help="LLM model to use")
    p.add_argument("--max-spend", type=float, help="Maximum spend budget in dollars")
    p.add_argument("--max-turns", type=int, help="Maximum conversation turns")
    p.add_argument("--async", action="store_true", dest="is_async",
                   help="Return immediately with thread ID")
    p.set_defaults(handler=handle)


def handle(args, project_path: str):
    from rye.tools.execute import ExecuteTool
    from rye.utils.resolvers import get_user_space

    params = parse_params(args.params_json)

    # Thread execution is always threaded
    params["thread"] = True

    if args.is_async:
        params["async"] = True

    # Build limit_overrides from thread flags
    limit_overrides = {}
    if args.model:
        limit_overrides["model"] = args.model
    if args.max_spend is not None:
        limit_overrides["spend"] = args.max_spend
    if args.max_turns is not None:
        limit_overrides["max_turns"] = args.max_turns
    if limit_overrides:
        params["limit_overrides"] = limit_overrides

    # Print what we're about to do
    print(
        f"[thread] spawning: {args.directive_id}"
        + (f" (model={args.model})" if args.model else "")
        + (f" (budget=${args.max_spend:.2f})" if args.max_spend else ""),
        file=sys.stderr,
    )

    tool = ExecuteTool(str(get_user_space()))
    result = run_async(tool.handle(
        item_type="directive",
        item_id=args.directive_id,
        project_path=project_path,
        parameters=params,
    ))
    print_result(result)
