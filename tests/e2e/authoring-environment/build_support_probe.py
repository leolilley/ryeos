# ryeos-tool:
#   category: ryeos/development/authoring-environment-production
#   version: "1.0.0"
#   description: E2E fixture for exact build-support subprocess closure
#   executor_id: tool:ryeos/development/authoring-environment-production/runtime
#   execution_protocol: protocol:ryeos/core/opaque
#   effects: live
#   workspace_access: immutable_current_generation
#   filesystem_authority: captured_execution
#   network_authority: isolated
#   config_schema:
#     type: object
#     properties: {}
#     additionalProperties: false
#   config_resolve:
#     type: single
#     spec:
#       path: development/ryeos/authoring-build-support.yaml
#       mode: first_match
#   external_content:
#     - id: producer-python
#       kind: tree
#       mode: pinned
#       digest: 800d4969489634cc3bbc5774bd9e99a330cdc23bbc1fd0fd231ec6a88ca9acdf
#       mount_root: execution_runtime
#       mount: producer-python
#     - id: authoring-build-support
#       kind: tree
#       mode: pinned
#       digest: f6bcd9d28b9bb3da0da3911cc8f021d75c326d38ac05329d38e2246ed7bea477
#       mount_root: execution_runtime
#       mount: authoring-build-support
#     - id: platform
#       kind: tree
#       mode: pinned
#       digest: 98bceddd5b4024d5963eeac8c579e6d4e79c24577980fa9f88bce9ae3151d316
#       mount_root: execution_runtime
#       mount: platform

"""Qualification fixture, not another compiler or a worker-granted operation.

Materialize and sign this fixture beside the existing runtime in a disposable
project. Bind the three exact inputs to that pinned project before invocation.
The script uses the production recipe's environment and process/log boundary;
all generated programs below are deliberately small E2E inputs.
"""

import hashlib
import json
from pathlib import Path
import shutil
import sys

from production import ElfTools, ordinary_member
from utilities import PLATFORM, SUPPORT, build_environment, checked_support, require_static, run


def main():
    if len(sys.argv) != 3 or sys.argv[1] != "--project-path":
        raise ValueError("missing admitted project context")
    project = Path(sys.argv[2])
    if not project.is_absolute() or project.is_symlink() or not project.is_dir():
        raise ValueError("invalid admitted project context")
    raw = sys.stdin.buffer.read(262145)
    if len(raw) > 262144:
        raise ValueError("probe parameters exceed bound")
    support = json.loads(raw)["resolved_config"]["support"]
    commands = checked_support(SUPPORT, support)
    # These negative assertions are part of the test: an accidental host
    # mount must not turn a missing subprocess dependency into a green run.
    for ambient in ("/bin/sh", "/usr/bin/make", "/usr/bin/cc", "/etc/ld.so.cache"):
        if Path(ambient).exists():
            raise ValueError("ambient runtime entered captured execution: " + ambient)
    products = project / "products"
    if not products.exists():
        products.mkdir(mode=0o700)
    ordinary_member(project, "products", directory=True)
    output = products / "build-support-probe"
    output.mkdir(mode=0o700)
    # The existing isolated /tmp owns compiler caches and build intermediates.
    # Do not put caches in retained products and invent capture-policy ignores
    # to compensate. Only explicit bounded diagnostics/products cross back.
    work = Path("/tmp/build-support-probe")
    work.mkdir(mode=0o700)
    for name in ("home", "tmp"):
        (work / name).mkdir(mode=0o700)
    env = build_environment(work, commands, PLATFORM)
    log = work / "probe.log"
    shell, make = str(commands["sh"]), str(commands["make"])
    # Load every selected executable, not only sh/make. Some POSIX commands
    # intentionally reject --help; loader/signal failures are never accepted.
    run([shell, "-c", '''
set -eu
for command do
  status=0
  "$command" --help || status=$?
  case "$status" in 0|1|2) ;; *) exit "$status" ;; esac
done
''', "helper-load-probe", *map(str, commands.values())], work, env, log)
    (work / "probe.c").write_text(
        '#include <stdio.h>\nint main(void) { puts("nested-build-ok"); return 0; }\n')
    (work / "nested.sh").write_text('set -eu\n"$CONFIG_SHELL" -c \'./probe\'\n')
    (work / "Makefile").write_text(
        'all: probe\n\t$(SHELL) nested.sh > actual.txt\n'
        '\tprintf "nested-build-ok\\n" > expected.txt\n'
        '\tcmp expected.txt actual.txt\n'
        'probe: probe.c\n\t$(CC) $(CFLAGS) $(LDFLAGS) -o probe probe.c\n')
    try:
        run([make, "-j1", f"SHELL={shell}", "all"], work, env, log)
    except Exception:
        shutil.copyfile(log, output / "probe.log")
        with log.open("rb") as stream:
            stream.seek(max(0, log.stat().st_size - 16384))
            sys.stderr.write(stream.read(16384).decode(errors="replace"))
        raise
    require_static(work / "probe", ElfTools(SUPPORT))
    run([str(commands["strip"]), "--strip-all", str(work / "probe")], work, env, log)
    require_static(work / "probe", ElfTools(SUPPORT))
    run([shell, str(work / "nested.sh")], work, env, log)
    expected = b"nested-build-ok\n"
    if (work / "actual.txt").read_bytes() != expected:
        raise ValueError("nested Make/shell/compiler fixture produced wrong output")
    for name in ("probe", "probe.c", "Makefile", "nested.sh", "actual.txt", "probe.log"):
        shutil.copyfile(work / name, output / name)
        (output / name).chmod(0o755 if name == "probe" else 0o644)
    print(json.dumps({"ok": True, "helpers_loaded": len(commands),
                      "nested_output_sha256": hashlib.sha256(expected).hexdigest(),
                      "static_output": True, "fresh_utility_build": False,
                      "output_path": "products/build-support-probe"}, sort_keys=True))


if __name__ == "__main__":
    main()
