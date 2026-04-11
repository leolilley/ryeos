"""ryeos-cli entry point.

Maps shell verbs to the three RYE primitives (fetch, execute, sign)
plus convenience verbs (thread, graph, test).

No MCP transport — imports ryeos directly as a Python library.
"""

import argparse
import sys

from rye_cli.verbs import fetch, execute, sign, thread, graph, test, remote, install, uninstall, daemon


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="rye",
        description="Terminal-native CLI for RYE OS",
    )
    parser.add_argument(
        "--project-path", "-p",
        default=".",
        help="Project root path (default: current directory)",
    )
    parser.add_argument(
        "--debug",
        action="store_true",
        help="Enable debug logging",
    )

    sub = parser.add_subparsers(dest="verb", required=True)

    fetch.register(sub)
    execute.register(sub)
    sign.register(sub)
    thread.register(sub)
    graph.register(sub)
    test.register(sub)
    remote.register(sub)
    install.register(sub)
    uninstall.register(sub)
    daemon.register(sub)

    return parser


def main():
    parser = build_parser()
    args = parser.parse_args()

    if args.debug:
        import logging
        import os
        os.environ["RYE_DEBUG"] = "true"
        logging.basicConfig(
            level=logging.DEBUG,
            format="[%(name)s] %(levelname)s: %(message)s",
            stream=sys.stderr,
        )

    # Resolve project path
    from pathlib import Path
    project_path = str(Path(args.project_path).resolve())

    # Dispatch to verb handler
    handler = args.handler
    handler(args, project_path)


if __name__ == "__main__":
    main()
