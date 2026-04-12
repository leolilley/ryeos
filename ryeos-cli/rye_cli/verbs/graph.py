"""rye graph run|step|validate <graph_id> [flags]

Graph-specific verb that translates flags into walker params.
"""

import sys

from rye_cli.output import daemon_execute, print_result, parse_params


def register(subparsers):
    p = subparsers.add_parser("graph", help="State-graph operations")
    sub = p.add_subparsers(dest="subcommand", required=True)

    run_p = sub.add_parser("run", help="Run a graph end-to-end")
    run_p.add_argument("graph_id", help="Graph tool ID")
    run_p.add_argument("--async", action="store_true", dest="is_async",
                       help="Spawn in background, return run ID")
    run_p.add_argument("--capabilities", default="rye.execute.tool.*",
                       help="Capability tokens (comma-separated, default: rye.execute.tool.*)")
    run_p.add_argument("--depth", type=int, default=5,
                       help="Max depth for nested execution (default: 5)")
    run_p.set_defaults(handler=handle_run)

    step_p = sub.add_parser("step", help="Execute a single node")
    step_p.add_argument("graph_id", help="Graph tool ID")
    step_p.add_argument("--node", required=True, help="Target node to execute")
    step_p.add_argument("--resume-from", dest="resume_from",
                        help="Resume state from a previous run ID")
    step_p.add_argument("--state", dest="state_json",
                        help="Inject state as JSON string")
    step_p.add_argument("--capabilities", default="rye.execute.tool.*",
                        help="Capability tokens (comma-separated)")
    step_p.set_defaults(handler=handle_step)

    val_p = sub.add_parser("validate", help="Static analysis without execution")
    val_p.add_argument("graph_id", help="Graph tool ID")
    val_p.set_defaults(handler=handle_validate)


def handle_run(args, project_path: str):
    raw = sys.stdin.read().strip() if not sys.stdin.isatty() else "{}"
    params = parse_params(raw)
    params["capabilities"] = [c.strip() for c in args.capabilities.split(",")]
    params["depth"] = args.depth

    if args.is_async:
        params["async"] = True

    print(f"[graph] running: {args.graph_id}", file=sys.stderr)
    result = daemon_execute(f"tool:{args.graph_id}", project_path=project_path, parameters=params)
    print_result(result)


def handle_step(args, project_path: str):
    params = {
        "node": args.node,
        "capabilities": [c.strip() for c in args.capabilities.split(",")],
    }

    if args.resume_from:
        params["resume"] = True
        params["graph_run_id"] = args.resume_from

    if args.state_json:
        params["inject_state"] = parse_params(args.state_json)

    print(f"[graph] stepping: {args.graph_id} → {args.node}", file=sys.stderr)
    result = daemon_execute(f"tool:{args.graph_id}", project_path=project_path, parameters=params)
    print_result(result)


def handle_validate(args, project_path: str):
    params = {"validate": True}
    print(f"[graph] validating: {args.graph_id}", file=sys.stderr)
    result = daemon_execute(f"tool:{args.graph_id}", project_path=project_path, parameters=params)
    print_result(result)
