# ryeos:signed:2026-11-13T06:25:04Z:7e14d1a5cb20da5ca566d8d5e7a0516bbaa89e6e49778c43c876d3b3ea79447d:kJVWCAFLSyYvJt/WlhP2OUiuvD23GKJqhYvlHRAn/YodJfaIcHvdJsPxsZCaJAexJLAOKhSqhq0kfpmW/dBUCw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
"""Bridge from one RyeOS Tool request to a development helper's command line.

The node delivers one JSON request on stdin: the schema-validated parameters,
plus `resolved_config` when the Tool selects a Config through
`config_resolve`. A helper reads its Config from a file, so the resolved value
is written to a private temporary file for the length of the call. The helper
in `lib/` stays the one implementation; an entry only maps typed parameters to
the arguments the helper already accepts. Nothing is read from the caller's
shell: a host-authority entry sees only the names its operator grant lists.
"""

from __future__ import annotations

import atexit
import json
import os
from pathlib import Path
import sys
import tempfile

MAX_REQUEST_BYTES = 262144


class EntryError(Exception):
    """A request the entry refuses before the helper runs."""


def read_request() -> dict:
    raw = sys.stdin.buffer.read(MAX_REQUEST_BYTES + 1)
    if len(raw) > MAX_REQUEST_BYTES:
        raise EntryError(f"request exceeds {MAX_REQUEST_BYTES} bytes")
    request = json.loads(raw or b"{}")
    if not isinstance(request, dict):
        raise EntryError("request is not a JSON object")
    return request


def config_file(request: dict) -> Path:
    """The Tool's resolved Config, written to a private temporary file."""
    config = request.get("resolved_config")
    if not isinstance(config, dict):
        raise EntryError("the Tool resolved no Config")
    if isinstance(config.get("value"), dict) and "source" in config:
        config = config["value"]
    descriptor, name = tempfile.mkstemp(prefix="ryeos-entry-config-", suffix=".json")
    atexit.register(_remove, name)
    with os.fdopen(descriptor, "w") as target:
        json.dump(config, target)
    return Path(name)


def absolute(value, field: str) -> str:
    """An absolute host path parameter; relative paths have no meaning here."""
    if not isinstance(value, str) or not value or not Path(value).is_absolute():
        raise EntryError(f"{field} must be an absolute path")
    return value


def project_path() -> Path | None:
    """The `--project-path` the RyeOS Python runtime passes, if any."""
    argv = sys.argv[1:]
    for index, value in enumerate(argv):
        if value == "--project-path" and index + 1 < len(argv):
            return Path(argv[index + 1])
    return None


def flag(argv: list[str], request: dict, key: str, option: str) -> None:
    if request.get(key) is True:
        argv.append(option)


def option(argv: list[str], request: dict, key: str, option_name: str) -> None:
    value = request.get(key)
    if value is not None:
        argv += [option_name, str(value)]


def run(main, argv: list[str]) -> None:
    """Run the helper's main with `argv`; exit with its status."""
    sys.argv = [sys.argv[0], *argv]
    sys.exit(main(argv))


def refuse(error: Exception, tool: str) -> None:
    print(f"{tool}: {error}", file=sys.stderr)
    sys.exit(2)


def _remove(name: str) -> None:
    try:
        os.unlink(name)
    except OSError:
        pass
