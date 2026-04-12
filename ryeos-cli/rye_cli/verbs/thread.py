"""rye thread <directive_id> [--model sonnet] [--max-spend 1.00] [--async]

Convenience alias for 'rye execute directive' with explicit thread-level flags.
Directive inputs are read as JSON from stdin.
"""

import sys

from rye_cli.output import daemon_execute, print_result, parse_params


def register(subparsers):
    p = subparsers.add_parser("thread", help="Spawn a directive thread")
    p.add_argument("directive_id", help="Directive ID (slash-separated path)")
    p.add_argument("--model", help="LLM model to use")
    p.add_argument("--max-spend", type=float, help="Maximum spend budget in dollars")
    p.add_argument("--max-turns", type=int, help="Maximum conversation turns")
    p.add_argument("--async", action="store_true", dest="is_async",
                   help="Return immediately with thread ID")
    p.set_defaults(handler=handle)


def handle(args, project_path: str):
    raw = sys.stdin.read().strip() if not sys.stdin.isatty() else "{}"
    params = parse_params(raw)

    params["thread"] = True

    if args.is_async:
        params["async"] = True

    if args.model:
        params["model"] = args.model
    if args.max_spend is not None:
        params["max_spend"] = args.max_spend
    if args.max_turns is not None:
        params["max_turns"] = args.max_turns

    print(
        f"[thread] spawning: {args.directive_id}"
        + (f" (model={args.model})" if args.model else "")
        + (f" (budget=${args.max_spend:.2f})" if args.max_spend else ""),
        file=sys.stderr,
    )

    result = daemon_execute(
        f"directive:{args.directive_id}",
        project_path=project_path,
        parameters=params,
    )
    print_result(result)
