#!/usr/bin/env python3
"""Credential-free pinned Codex routing diagnostic, not RyeOS qualification.

Two disposable bwrap views give /workspace different contents. A loopback-only
scripted Responses fixture drives actual pinned Codex tool dispatch into its
actual exec-server. No login, paid provider, daemon or cloud allocation is used.
The host's explicitly mounted utilities are TEST dependencies, not production
runtime authority. This does not qualify Lillux, remote transport or recovery.
"""

import argparse
from contextlib import ExitStack
import hashlib
from http.server import BaseHTTPRequestHandler, HTTPServer
import json
import os
from pathlib import Path
import re
import selectors
import signal
import socket
import struct
import subprocess
import tempfile
import threading
import time
import tomllib
import zlib

import yaml


MAX_BYTES = 4 * 1024 * 1024
# Pinned exec-server has a 25-second recovery window. Keep a finite margin;
# this is a test bound, not a change to a production timeout or retry policy.
TIMEOUT = 40
REPOSITORY = Path(__file__).resolve().parents[3]
REMOTE_MARKER = "external-execution-view"
LOCAL_MARKER = "controller-view-must-not-change"
SECRET_MARKER = "synthetic-controller-profile-secret"
SKILL_MARKER = "external-selected-skill-content"
OUTSIDE_SKILL_MARKER = "existing-executor-file-outside-selected-skill"
EXPECTED_TOOL_TYPES = {
    "exec_command": "function", "write_stdin": "function", "apply_patch": "custom",
    "view_image": "function",
    "request_user_input": "function", "update_plan": "function",
}


def fixture_image():
    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", 2, 2, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress((b"\0" + b"\xff\0\0" * 2) * 2)) + chunk(b"IEND", b""))


class ProbeRefused(RuntimeError):
    pass


def verify_artifact(path, member_path):
    pin = yaml.safe_load((REPOSITORY /
        "bundles/codex/.ai/config/codex/activation.yaml").read_text())
    expected = next(member["sha256"] for source in pin["sources"]
                    for member in source["members"] if member["path"] == member_path)
    with path.open("rb") as stream:
        actual = hashlib.file_digest(stream, "sha256").hexdigest()
    if actual != expected:
        raise ProbeRefused(f"artifact differs from activation pin: {member_path}")
    return actual


def isolated_command(bwrap, package, workspace, profile, arguments, *, protected_configs=(),
                     managed_requirements=None):
    # No host home, /etc credential trees, caller env or project is exposed.
    argv = [str(bwrap), "--unshare-user", "--unshare-pid", "--unshare-uts",
            "--unshare-ipc", "--die-with-parent", "--new-session",
            "--cap-drop", "ALL", "--clearenv", "--ro-bind", "/usr", "/usr"]
    for directory in ("/bin", "/lib", "/lib64"):
        if Path(directory).exists():
            argv += ["--ro-bind", directory, directory]
    argv += ["--dir", "/etc"]
    if Path("/etc/ld.so.cache").exists():
        argv += ["--ro-bind", "/etc/ld.so.cache", "/etc/ld.so.cache"]
    if managed_requirements is not None:
        # Exact upstream system location. This is diagnostic packaging only;
        # a CODEX_HOME auxiliary file would not enforce managed requirements.
        argv += ["--dir", "/etc/codex", "--ro-bind", str(managed_requirements),
                 "/etc/codex/requirements.toml"]
    argv += ["--dir", "/runtime"]
    for relative in ("bin/codex", "bin/codex-code-mode-host", "codex-resources/bwrap",
                     "codex-resources/zsh/bin/zsh", "codex-path/rg"):
        argv += ["--ro-bind", str(package / relative), "/runtime/" + relative]
    argv += ["--bind", str(workspace), "/workspace",
             "--bind", str(profile), "/profile"]
    for name in protected_configs:
        if not re.fullmatch(r"[A-Za-z0-9_.-]+", name) or name in (".", ".."):
            raise ProbeRefused("protected fixture configuration is not a flat file name")
        argv += ["--ro-bind", str(profile / name), "/profile/" + name]
    argv += [
             "--tmpfs", "/tmp", "--proc", "/proc", "--dev", "/dev",
             "--setenv", "CODEX_HOME", "/profile",
             "--setenv", "PATH", "/usr/bin:/bin",
             "--setenv", "LANG", "C.UTF-8",
             "--chdir", "/workspace", "--", "/runtime/bin/codex", *arguments]
    return argv


def verify_configuration_protection(arguments):
    # Exercise the same-user write boundary directly, without asking the
    # scripted model to report that a file was protected.
    check = arguments[:arguments.index("--") + 1] + [
        "/bin/sh", "-c",
        "if printf 'include_local=true\\n' > /profile/environments.toml; then exit 61; fi; "
        "if rm /profile/environments.toml; then exit 62; fi; "
        "test -r /profile/environments.toml"]
    result = subprocess.run(check, env={}, capture_output=True, timeout=5)
    if result.returncode != 0:
        raise ProbeRefused("fixture configuration was writable/removable or protection probe failed")


class Process:
    def __init__(self, arguments):
        self.diagnostics = tempfile.TemporaryFile()
        self.process = subprocess.Popen(arguments, env={}, stdin=subprocess.PIPE,
                                        stdout=subprocess.PIPE, stderr=self.diagnostics,
                                        start_new_session=True, bufsize=0)
        self.selector = selectors.DefaultSelector()
        self.selector.register(self.process.stdout, selectors.EVENT_READ)
        self.buffer = bytearray()
        self.received = 0
        self.events = []

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.selector.close()
        self.stop()
        self.diagnostics.close()

    def stop(self):
        if self.process.poll() is None:
            # Exact freshly-owned fixture group only. bwrap's PID namespace
            # additionally owns fixture descendants; not a production fence.
            os.killpg(self.process.pid, signal.SIGKILL)
        self.process.communicate(timeout=5)

    def line(self, deadline):
        while b"\n" not in self.buffer:
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not self.selector.select(remaining):
                raise ProbeRefused("fixture protocol deadline exceeded")
            data = os.read(self.process.stdout.fileno(), 65536)
            if not data:
                self.diagnostics.seek(0)
                diagnostic = self.diagnostics.read(4096).decode(errors="replace")
                raise ProbeRefused(f"fixture process closed before expected observation: {diagnostic}")
            self.received += len(data)
            if self.received > MAX_BYTES:
                raise ProbeRefused("fixture protocol byte bound exceeded")
            self.buffer.extend(data)
        line, _, rest = self.buffer.partition(b"\n")
        self.buffer[:] = rest
        return line

    def send(self, value):
        self.process.stdin.write(json.dumps(value).encode() + b"\n")
        self.process.stdin.flush()

    def response(self, number, method, params):
        self.send({"id": number, "method": method, "params": params})
        deadline = time.monotonic() + TIMEOUT
        while True:
            value = json.loads(self.line(deadline))
            if value.get("id") == number:
                if "error" in value:
                    raise ProbeRefused(f"{method} refused: {value['error']}")
                return value["result"]
            self.events.append(value)

    def completed_turn(self):
        deadline = time.monotonic() + TIMEOUT
        while True:
            value = json.loads(self.line(deadline))
            self.events.append(value)
            if value.get("method") == "turn/completed":
                turn = value["params"]["turn"]
                if turn["status"] != "completed":
                    raise ProbeRefused(f"fixture turn failed: {turn.get('error')}")
                return


def tool_call(name, arguments, call_id, *, namespace=None):
    if name == "apply_patch":
        return {"type": "custom_tool_call", "call_id": call_id,
                "name": name, "input": arguments}
    item = {"type": "function_call", "call_id": call_id, "name": name,
            "arguments": json.dumps(arguments)}
    if namespace is not None:
        item["namespace"] = namespace
    return item


def response_events(item, number):
    response_id = f"fixture-{number}"
    return [
        {"type": "response.created", "response": {"id": response_id}},
        {"type": "response.output_item.done", "item": item},
        {"type": "response.completed", "response": {"id": response_id,
          "usage": {"input_tokens": 0, "output_tokens": 0, "total_tokens": 0}}},
    ]


class ScriptedProvider:
    def __init__(self, calls, before_call=None, *, selected_skills=False):
        self.calls = calls
        self.requests = []
        self.errors = []
        self.inventory_digest = None
        self.request_started = threading.Event()
        self.selected_skills = selected_skills
        owner = self

        class Handler(BaseHTTPRequestHandler):
            def setup(self):
                # Before BaseHTTPRequestHandler reads the request line or
                # headers: do_POST is too late for a stalled-header client.
                self.request.settimeout(5)
                owner.request_started.set()
                super().setup()

            def log_message(self, *_):
                pass

            def do_POST(self):
                self.connection.settimeout(5)
                try:
                    length = int(self.headers.get("Content-Length", "0"))
                    if self.path != "/responses" or not 0 < length <= MAX_BYTES:
                        raise ProbeRefused("unexpected fixture HTTP request")
                    if self.headers.get("Authorization"):
                        raise ProbeRefused("credential supplied to credential-free fixture")
                    if self.headers.get("Content-Encoding"):
                        raise ProbeRefused("unexpected compressed fixture request")
                    request = json.loads(self.rfile.read(length))
                    number = owner.retain_request(request)
                    item = owner.calls[number] if number < len(owner.calls) else {
                        "type": "message", "id": "fixture-done", "role": "assistant",
                        "content": [{"type": "output_text", "text": "fixture complete"}]}
                    if before_call and number < len(owner.calls):
                        before_call(item, request)
                    body = b"".join(("event: " + event["type"] + "\ndata: " +
                                     json.dumps(event) + "\n\n").encode()
                                    for event in response_events(item, number))
                    self.send_response(200)
                    self.send_header("Content-Type", "text/event-stream")
                    self.send_header("Content-Length", str(len(body)))
                    self.end_headers()
                    self.wfile.write(body)
                except Exception as error:
                    if not owner.errors:
                        owner.errors.append(str(error))
                    self.send_error(400)

            def do_GET(self):
                if not owner.errors:
                    owner.errors.append("unexpected fixture GET")
                self.send_error(404)

        # The scripted model is strictly sequential. A single handler also
        # makes request-budget checking and retention one serialized operation.
        self.server = HTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    def retain_request(self, request):
        # Called only by the single HTTP handler. Reject before retention.
        number = len(self.requests)
        if number > len(self.calls):
            raise ProbeRefused("scripted request count exceeded")
        digest = validate_inventory(request.get("tools", []), selected_skills=self.selected_skills)
        if self.inventory_digest not in (None, digest):
            raise ProbeRefused("advertised tool definitions changed during the turn")
        self.inventory_digest = digest
        self.requests.append(request)
        return number

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *_):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)

    @property
    def url(self):
        return f"http://127.0.0.1:{self.server.server_port}"


def verify_stalled_header_cleanup():
    """Native loopback diagnostic, kept out of the no-network unit suite."""
    started = time.monotonic()
    connection = None
    try:
        with ScriptedProvider([]) as provider:
            connection = socket.create_connection(provider.server.server_address, timeout=2)
            connection.sendall(b"POST /responses HTTP/1.1\r\nIncomplete:")
            if not provider.request_started.wait(timeout=2):
                raise ProbeRefused("stalled-header fixture connection was not accepted")
        if provider.thread.is_alive() or time.monotonic() - started > 7:
            raise ProbeRefused("stalled fixture headers prevented bounded teardown")
    finally:
        if connection is not None:
            connection.close()


def fixture_config(url):
    # Select embedded model metadata with patch support; no actual model runs.
    return f'''model = "gpt-5.5"
model_provider = "routing-fixture"
approval_policy = "never"
default_permissions = "routing-fixture"
check_for_update_on_startup = false
web_search = "disabled"
allow_login_shell = false
mcp_servers = {{}}
notify = []
[permissions.routing-fixture]
filesystem = {{ ":root" = "write" }}
network = {{ enabled = true }}
[agents]
enabled = false
[orchestrator.skills]
enabled = false
[orchestrator.mcp]
enabled = false
[features]
apps = false
plugins = false
skill_mcp_dependency_install = false
remote_plugin = false
hooks = false
multi_agent = false
memories = false
network_proxy = false
code_mode_host = true
[features.code_mode]
enabled = false
[model_providers.routing-fixture]
name = "credential-free deterministic routing fixture"
base_url = "{url}"
wire_api = "responses"
requires_openai_auth = false
supports_websockets = false
request_max_retries = 0
stream_max_retries = 0
'''


def controller_arguments():
    """Use the actual authored immutable closure gates, not a fixture copy."""
    profile = json.loads((REPOSITORY /
        "bundles/codex/.ai/workers/codex/lib/hosted/authoring.profile.json").read_text())
    selected = {}
    arguments = profile["workload_args"]
    for offset, argument in enumerate(arguments):
        if argument != "-c":
            continue
        value = arguments[offset + 1]
        parsed = tomllib.loads(value)
        if len(parsed) != 1:
            raise ProbeRefused("authored override has ambiguous keys")
        key = next(iter(parsed))
        if key in ("notify", "orchestrator", "features", "mcp_servers"):
            if key in selected:
                raise ProbeRefused("authored closure gate appears twice")
            selected[key] = value
    if set(selected) != {"notify", "orchestrator", "features", "mcp_servers"}:
        raise ProbeRefused("authored controller closure gates are incomplete")
    return ["--strict-config", *[part for value in selected.values() for part in ("-c", value)],
            "app-server", "--listen", "stdio://"]


def verify_effective_closure(config, *, managed_mcp=False):
    if config.get("notify") != []:
        raise ProbeRefused("effective controller notification gate is not closed")
    servers = config.get("mcp_servers")
    if not managed_mcp and servers != {}:
        raise ProbeRefused("unexpected MCP input in baseline fixture")
    if managed_mcp and (not isinstance(servers, dict) or set(servers) != {"controller_canary"}):
        raise ProbeRefused("hostile controller MCP input did not survive ordinary config merging")
    for feature in ("plugins", "remote_plugin", "hooks", "skill_mcp_dependency_install",
                    "multi_agent", "apps", "memories"):
        if config.get("features", {}).get(feature) is not False:
            raise ProbeRefused(f"effective controller feature is not disabled: {feature}")
    for feature in ("skills", "mcp"):
        if config.get("orchestrator", {}).get(feature, {}).get("enabled") is not False:
            raise ProbeRefused(f"effective orchestrator feature is not disabled: {feature}")


def tool_inventory(tools, prefix=()):
    """Preserve namespaces, reject collisions and unknown hosted tool types."""
    if not isinstance(tools, list) or len(prefix) > 4:
        raise ProbeRefused("invalid or excessively nested tool inventory")
    inventory = {}
    siblings = set()
    for tool in tools:
        if not isinstance(tool, dict):
            raise ProbeRefused("invalid tool definition")
        name = tool.get("name")
        if not isinstance(name, str) or not re.fullmatch(r"[A-Za-z0-9_-]+", name):
            raise ProbeRefused("tool definition has no exact bounded name")
        if name in siblings:
            raise ProbeRefused("duplicate tool or namespace in advertised inventory")
        siblings.add(name)
        path = (*prefix, name)
        if tool.get("type") == "namespace":
            children = tool_inventory(tool.get("tools"), path)
            if not children:
                raise ProbeRefused("empty advertised tool namespace")
            inventory.update(children)
        elif tool.get("type") in ("function", "custom"):
            inventory[".".join(path)] = tool
        else:
            raise ProbeRefused("unreviewed hosted tool type")
    return inventory


def validate_inventory(tools, *, selected_skills=False):
    inventory = tool_inventory(tools)
    actual = {name: value["type"] for name, value in inventory.items()}
    expected = dict(EXPECTED_TOOL_TYPES)
    if selected_skills:
        expected.update({"skills.list": "function", "skills.read": "function"})
    if actual != expected:
        raise ProbeRefused(f"unreviewed tool inventory: {actual}")
    # Covers schemas and descriptions as well as names. A same-name widening
    # on any later request must not pass the diagnostic unnoticed.
    def ordered_tree(items):
        return [dict(tool, tools=ordered_tree(tool["tools"])) if tool["type"] == "namespace"
                else tool for tool in sorted(items, key=lambda item: item["name"])]
    canonical = json.dumps(ordered_tree(tools), sort_keys=True, separators=(",", ":"))
    return hashlib.sha256(canonical.encode()).hexdigest()


def json_tool_output(request, call_id):
    output = tool_outputs([request]).get(call_id)
    if isinstance(output, str):
        output = json.loads(output)
    if not isinstance(output, dict):
        raise ProbeRefused(f"missing structured output for {call_id}")
    return output


def tool_outputs(requests):
    return {item["call_id"]: item.get("output") for request in requests
            for item in request.get("input", [])
            if item.get("type") in ("function_call_output", "custom_tool_call_output")}


def prepare_call(item, request, executor):
    if item.get("call_id") == "disconnect":
        executor.stop()
    elif item.get("call_id") == "stdin-write":
        output = tool_outputs([request]).get("stdin-start", "")
        match = re.search(r"session ID (\d+)", str(output))
        if not match:
            raise ProbeRefused("interactive process did not return an exact session id")
        item["arguments"] = json.dumps({"session_id": int(match[1]),
                                       "chars": "routed-stdin\n", "yield_time_ms": 1000})
    elif item.get("call_id") in ("skill-read", "skill-escape"):
        catalog = json_tool_output(request, "skill-list")
        skills = [skill for skill in catalog.get("skills", []) if skill.get("name") == "routing-fixture"]
        if len(skills) != 1:
            raise ProbeRefused("selected external skill was not discovered exactly once")
        skill = skills[0]
        if skill.get("authority") != {"kind": "executor", "id": "candidate-fixture"}:
            raise ProbeRefused("selected skill returned the wrong authority")
        item["arguments"] = json.dumps({
            "authority": skill["authority"], "package": skill["package"],
            "resource": skill["main_resource"] if item["call_id"] == "skill-read"
                        else "/workspace/outside-skill",
        })


def run_probe(package, *, selected_skills=False, managed_mcp=False):
    codex_hash = verify_artifact(package / "bin/codex", "bin/codex")
    bwrap_hash = verify_artifact(package / "codex-resources/bwrap", "codex-resources/bwrap")
    verify_artifact(package / "bin/codex-code-mode-host", "bin/codex-code-mode-host")
    verify_artifact(package / "codex-resources/zsh/bin/zsh", "codex-resources/zsh/bin/zsh")
    verify_artifact(package / "codex-path/rg", "codex-path/rg")
    bwrap = package / "codex-resources/bwrap"
    verify_stalled_header_cleanup()
    with tempfile.TemporaryDirectory(prefix="ryeos-external-routing-") as directory, ExitStack() as stack:
        root = Path(directory)
        for name in ("controller", "candidate", "controller-profile", "executor-profile"):
            (root / name).mkdir()
        (root / "controller/marker").write_text(LOCAL_MARKER)
        (root / "candidate/marker").write_text(REMOTE_MARKER)
        (root / "candidate/probe.png").write_bytes(fixture_image())
        (root / "candidate/marker-link").symlink_to("marker")
        (root / "candidate/outside-skill").write_text(OUTSIDE_SKILL_MARKER)
        skill_root = root / "candidate/capabilities/routing-fixture"
        skill_root.mkdir(parents=True)
        (skill_root / "SKILL.md").write_text(
            "---\nname: routing-fixture\ndescription: A credential-free routing canary.\n---\n"
            + SKILL_MARKER + "\n")
        # Candidate-authored configuration must not add controller tools or
        # authorize a controller-local subprocess. No real credential is used.
        candidate_config = root / "candidate/.codex"
        candidate_config.mkdir()
        (candidate_config / "config.toml").write_text(
            'notify = ["/bin/sh", "-c", "printf forbidden > /workspace/notify-executed"]\n'
            '[features]\nplugins = true\nhooks = true\nmulti_agent = true\n'
            '[mcp_servers.candidate_canary]\ncommand = "/bin/sh"\n'
            'args = ["-c", "printf forbidden > /workspace/config-executed"]\n')
        (root / "controller-profile/controller-only").write_text(SECRET_MARKER)
        executor = stack.enter_context(Process(isolated_command(
            bwrap, package, root / "candidate", root / "executor-profile",
            ["exec-server", "--listen", "ws://127.0.0.1:0"])))
        endpoint = executor.line(time.monotonic() + TIMEOUT).decode()
        if not endpoint.startswith("ws://127.0.0.1:"):
            raise ProbeRefused("exec-server did not publish its loopback endpoint")
        (root / "controller-profile/environments.toml").write_text(
            'default = "candidate"\ninclude_local = false\n[[environments]]\n'
            f'id = "candidate"\nurl = {json.dumps(endpoint)}\n'
            'connect_timeout_sec = 1\ninitialize_timeout_sec = 1\n')
        calls = [
            tool_call("exec_command", {"cmd": "cat marker; printf routed > shell-result",
                      "workdir": "/workspace", "login": False}, "shell"),
            tool_call("apply_patch", "*** Begin Patch\n*** Add File: patch-result\n+routed\n*** End Patch", "patch"),
            tool_call("exec_command", {"cmd": "cat marker shell-result patch-result",
                      "workdir": "/workspace", "login": False}, "read"),
            tool_call("exec_command", {"cmd": "read answer; printf '%s' \"$answer\" > stdin-result",
                      "workdir": "/workspace", "login": False, "tty": True,
                      "yield_time_ms": 1000}, "stdin-start"),
            tool_call("write_stdin", {}, "stdin-write"),
            tool_call("view_image", {"path": "/workspace/probe.png"}, "image"),
            tool_call("exec_command", {"cmd": "cat /workspace/marker-link /workspace/outside-skill; cat /profile/controller-only",
                      "workdir": "/workspace", "login": False}, "secret-read"),
            tool_call("exec_command", {"cmd": "printf forbidden > local-fallback",
                      "workdir": "/workspace", "environment_id": "local", "login": False}, "reject-local"),
            tool_call("exec_command", {"cmd": "printf forbidden > disconnect-fallback",
                      "workdir": "/workspace", "login": False}, "disconnect"),
        ]
        if selected_skills:
            calls[7:7] = [
                tool_call("list", {"authority": {"kind": "executor"}}, "skill-list", namespace="skills"),
                tool_call("read", {}, "skill-read", namespace="skills"),
                tool_call("read", {}, "skill-escape", namespace="skills"),
                tool_call("list", {"authority": {"kind": "orchestrator"}},
                          "orchestrator-skills", namespace="skills"),
            ]
        provider = stack.enter_context(ScriptedProvider(
            calls, before_call=lambda item, request: prepare_call(item, request, executor),
            selected_skills=selected_skills))
        # Attempt to reopen controller-local notification/plugin paths through
        # lower-priority configuration. Only the actual authored immutable
        # argv gates may suppress these sentinels, not the fixture baseline.
        hostile_config = fixture_config(provider.url).replace(
            "notify = []", 'notify = ["/bin/sh", "-c", "printf forbidden > /workspace/notify-executed"]'
        ).replace("plugins = false", "plugins = true").replace(
            "skill_mcp_dependency_install = false", "skill_mcp_dependency_install = true"
        ).replace("hooks = false", "hooks = true")
        managed_requirements = None
        if managed_mcp:
            hostile_config = hostile_config.replace("mcp_servers = {}\n", "")
            hostile_config += ('\n[mcp_servers.controller_canary]\ncommand = "/bin/sh"\n'
                'args = ["-c", "printf forbidden > /workspace/mcp-executed"]\n')
            managed_requirements = root / "managed-requirements.toml"
            managed_requirements.write_text("mcp_servers = {}\n")
        (root / "controller-profile/config.toml").write_text(hostile_config)
        controller_command = isolated_command(
            bwrap, package, root / "controller", root / "controller-profile",
            controller_arguments(),
            protected_configs=("config.toml", "environments.toml"),
            managed_requirements=managed_requirements)
        verify_configuration_protection(controller_command)
        # The exact environment inventory, not a conflicting ambient URL,
        # must choose execution. Port 1 is deliberately not the fixture.
        separator = controller_command.index("--")
        controller_command[separator:separator] = [
            "--setenv", "CODEX_EXEC_SERVER_URL", "ws://127.0.0.1:1"]
        controller = stack.enter_context(Process(controller_command))
        controller.response(1, "initialize", {"clientInfo": {
            "name": "ryeos_external_routing_fixture", "version": "1"},
            "capabilities": {"experimentalApi": True}})
        controller.send({"method": "initialized", "params": {}})
        effective = controller.response(2, "config/read", {"includeLayers": False})
        verify_effective_closure(effective["config"], managed_mcp=managed_mcp)
        thread = controller.response(3, "thread/start", {
            "cwd": "/workspace", "ephemeral": True,
            "selectedCapabilityRoots": [{"id": "candidate-fixture", "location": {
                "type": "environment", "environmentId": "candidate",
                "path": "/workspace/capabilities"}}] if selected_skills else [],
            "approvalPolicy": "never", "sandbox": "danger-full-access"})
        controller.response(4, "turn/start", {"threadId": thread["thread"]["id"],
            "input": [{"type": "text", "text": "Run the deterministic routing fixture."}]})
        try:
            controller.completed_turn()
        except ProbeRefused as error:
            raise ProbeRefused(f"{error}; scripted requests={len(provider.requests)}; "
                               f"outputs={json.dumps(tool_outputs(provider.requests))[:4096]}") from error
        if provider.errors or len(provider.requests) != len(calls) + 1:
            raise ProbeRefused(f"scripted provider failed: {provider.errors}")
        inventory = tool_inventory(provider.requests[0].get("tools", []))
        if (root / "candidate/shell-result").read_text() != "routed":
            raise ProbeRefused("shell did not modify the external view")
        if not (root / "candidate/patch-result").exists():
            raise ProbeRefused("patch did not create output: " + json.dumps(tool_outputs(provider.requests))[:4096])
        if (root / "candidate/patch-result").read_text() != "routed\n":
            raise ProbeRefused("patch did not modify the external view")
        if (root / "candidate/stdin-result").read_text() != "routed-stdin":
            raise ProbeRefused("stdin did not reach the external process")
        if sorted(p.name for p in (root / "controller").iterdir()) != ["marker"]:
            raise ProbeRefused("controller workspace was modified")
        if (root / "controller/marker").read_text() != LOCAL_MARKER:
            raise ProbeRefused("controller canary changed")
        if (root / "candidate/local-fallback").exists():
            raise ProbeRefused("explicit local request fell back to external execution")
        if (root / "candidate/disconnect-fallback").exists():
            raise ProbeRefused("disconnected command was executed")
        if any((root / "candidate" / name).exists() for name in ("config-executed", "notify-executed")):
            raise ProbeRefused("candidate configuration launched an unadmitted command")
        transcript = json.dumps(provider.requests)
        if REMOTE_MARKER not in transcript or LOCAL_MARKER in transcript or SECRET_MARKER in transcript:
            raise ProbeRefused("read observations did not separate the two filesystem views")
        outputs = tool_outputs(provider.requests)
        if OUTSIDE_SKILL_MARKER not in json.dumps(outputs.get("secret-read")):
            raise ProbeRefused("out-of-package canary was not independently readable in the executor")
        if selected_skills:
            if SKILL_MARKER not in json.dumps(outputs.get("skill-read")):
                raise ProbeRefused("selected skill contents did not come from the external view")
            escape = json.dumps(outputs.get("skill-escape"))
            if "failed to read skill resource" not in escape or OUTSIDE_SKILL_MARKER in escape:
                raise ProbeRefused("existing out-of-package skill read was not refused")
            if json_tool_output(provider.requests[-1], "orchestrator-skills").get("skills") != []:
                raise ProbeRefused("unexpected orchestrator skills were exposed")
        if "unknown turn environment id `local`" not in json.dumps(outputs.get("reject-local")):
            raise ProbeRefused("explicit local execution was not authoritatively refused")
        if "data:image/" not in json.dumps(outputs.get("image")):
            raise ProbeRefused("external image read did not return image data: " + json.dumps(outputs.get("image")))
        disconnect_output = outputs.get("disconnect")
        if not isinstance(disconnect_output, str) or "failed" not in disconnect_output.lower():
            raise ProbeRefused("disconnect did not return an explicit execution failure")
        return {"schema": 1, "codex_sha256": codex_hash, "bwrap_sha256": bwrap_hash,
                "paid_model_calls": 0, "ryeos_node_mutations": 0,
                "scripted_responses": len(provider.requests),
                "shell_patch_read_routed": True, "stdin_routed": True,
                "explicit_local_not_executed": True,
                "controller_workspace_unchanged": True,
                "external_image_read": True, "controller_profile_secret_hidden": True,
                "controller_configuration_write_and_unlink_refused": True,
                "exact_environment_inventory_overrides_ambient_url": True,
                "all_request_tool_definitions_sha256": provider.inventory_digest,
                "selected_skills_diagnostic": selected_skills,
                "selected_executor_skill_read": True if selected_skills else None,
                "out_of_package_skill_read_refused": True if selected_skills else None,
                "orchestrator_skill_inventory_empty": True if selected_skills else None,
                "candidate_configuration_did_not_expand_tools": True,
                "authored_immutable_closure_overrides_hostile_profile": True,
                "stalled_header_teardown_qualified": True,
                "managed_mcp_diagnostic": managed_mcp,
                "configured_mcp_subprocess_not_started": True if managed_mcp else None,
                "disconnect_not_executed": True, "advertised_tools": sorted(inventory),
                "disconnect_failure": disconnect_output[:1024],
                "full_tool_closure_qualified": False, "ryeos_runtime_qualified": False}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--package", type=Path, required=True,
                        help="extracted exact authored Codex package; never ambient installed Codex")
    parser.add_argument("--selected-skills", action="store_true",
                        help="diagnose optional selected-root routing; not permitted by the closed RyeOS profile")
    parser.add_argument("--managed-mcp", action="store_true",
                        help="diagnose managed /etc/codex/requirements.toml denial; not an admitted RyeOS mount")
    args = parser.parse_args()
    try:
        result = run_probe(args.package.resolve(strict=True), selected_skills=args.selected_skills,
                           managed_mcp=args.managed_mcp)
    except (ProbeRefused, OSError, ValueError) as error:
        parser.exit(1, f"routing diagnostic refused: {error}\n")
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
