# ryeos-tool:
#   category: ryeos/development/authoring-environment-production
#   version: "1.0.0"
#   description: E2E fixture for the canonical fresh utility build recipe
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
#     type: multi
#     specs:
#       - path: development/ryeos/authoring-build-support.yaml
#         mode: first_match
#       - path: development/ryeos/authoring-environment-inputs.yaml
#         mode: first_match
#       - path: development/ryeos/authoring-utility-sources.yaml
#         mode: first_match
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
#     - id: source-inputs
#       kind: tree
#       mode: pinned
#       digest: a6b454a400b503830c71767a91abf649d61979ad1031ffe9840277b3d06fb616
#       mount_root: execution_runtime
#       mount: authoring-source-inputs
#     - id: authoring-tools
#       kind: tree
#       mode: pinned
#       digest: 1ca7a7fe9ecc1d19c38c986ba9df435ff7bbe6d4ba020f2643564b8bba0c84e6
#       mount_root: execution_runtime
#       mount: authoring-tools

"""Independent fresh utility/runtime E2E; shared production owns the build."""
import json

from production import ordinary_member
from utilities import build_environment, checked_support, run, PLATFORM, SUPPORT
from utility_production import parse_request, produce, SUPPORT_CONFIG, WORK, OUTPUT


def main():
    project, resolved = parse_request()
    result = produce(project, resolved)
    # The separately bound authoring tree supplies only Git's final shell.
    # It is not the source of newly compiled outputs or a worker grant.
    env = build_environment(WORK, checked_support(SUPPORT, resolved[SUPPORT_CONFIG]["support"]), PLATFORM)
    env.update({"GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null"})
    run([str(project / OUTPUT / "bin/git"), "-c",
         "alias.ryeos-probe=!printf verified > git-shell.txt", "ryeos-probe"],
        WORK, env, project / "products/utility-runtime-probe.log")
    if ordinary_member(WORK, "git-shell.txt").read_bytes() != b"verified":
        raise ValueError("fresh Git did not execute the selected authoring shell")
    print(json.dumps({**result, "worker_driven": False,
                      "git_runtime_shell_verified": True}, sort_keys=True))


if __name__ == "__main__":
    main()
