"""rye init — Bootstrap Rye OS on a fresh machine.

Runs ryeosd --init-only to create the node space layout, generate the
signing key, write the public identity document, and bootstrap self-trust.
"""

import os
import sys

from rye_cli.output import die


def register(subparsers):
    p = subparsers.add_parser("init", help="Bootstrap Rye OS (generate keys, create state)")
    p.add_argument(
        "--force",
        action="store_true",
        help="Overwrite existing key, config, and identity",
    )
    p.set_defaults(handler=_handle_init)


def _handle_init(args, project_path: str):
    """Run ryeosd --init-only."""
    cmd = ["ryeosd", "--init-only"]
    if args.force:
        cmd.append("--force")
    try:
        os.execvp("ryeosd", cmd)
    except FileNotFoundError:
        die("ryeosd not found — is it installed and on PATH?")
