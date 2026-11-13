#!/usr/bin/env python3
# ryeos:signed:2026-11-13T06:25:04Z:7626ac622b5956fe31c5de0f7b55351cdbdb5ca2400cce6d7a24479316b05f0a:oIHCamaFVnVE9g7XPRbe5Z50ltixwRA/B17Wo+bMSEaUQkoEPw/F0Bpn3EhBrvppsbO106Z353MlI+DQ0+OPDA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: ryeos/development/render-provider-shape-check
#   version: "1.0.0"
#   description: Check Render's current sandbox response still fits the render-sandbox adapter's strict parser
#   executor_id: tool:ryeos/core/runtimes/python/script
#   execution_protocol: protocol:ryeos/core/opaque
#   effects: live
#   required_secrets: [RENDER_API_KEY]
#   config_resolve:
#     type: single
#     spec:
#       path: development/ryeos/render-provider-shape.yaml
#       mode: first_match
#   config_schema:
#     type: object
#     properties:
#       owner_id:
#         type: string
#         description: Render owner whose newest terminated sandbox is read
#     additionalProperties: false

"""Ordinary sandboxed Tool for render-provider-shape-check.

One read-only HTTPS GET with the `RENDER_API_KEY` secret, checked against the
adapter source in the project. It needs no host access: the key comes through
`required_secrets`, never the worktree's `.env`.
"""

from pathlib import Path
import importlib.util
import sys


def _load(path: Path, name: str):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


if __name__ == "__main__":
    here = Path(__file__).resolve().parent
    entry = _load(here.parent / "dev-host/lib/node_entry.py", "node_entry")
    try:
        request = entry.read_request()
        project = entry.project_path()
        if project is None:
            raise entry.EntryError("the runtime passed no project path")
        argv = ["--config-file", str(entry.config_file(request)), "--worktree", str(project)]
        entry.option(argv, request, "owner_id", "--owner-id")
    except (entry.EntryError, ValueError) as error:
        entry.refuse(error, "render-provider-shape-check")
    sys.path.insert(0, str(here / "lib"))
    import render_provider_shape_check
    entry.run(render_provider_shape_check.main, argv)
