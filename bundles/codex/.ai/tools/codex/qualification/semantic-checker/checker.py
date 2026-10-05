"""Pure Codex Q conversation interpretation; no launch or qualification authority.

check_conversation(envelope) consumes host-authenticated bounded
JSON objects. The host still owns source/runtime admission, chunk/capture custody,
record/purpose validation, deadlines, process settlement and result issuance.
There is deliberately no CLI, FD reader, filesystem, process or network code.

Behavior map to consumer_worker_semantics.rs:
  expectation/fixture -> _expectation/_fixture; setup -> _setup;
  request binding -> _canary; lifecycle -> _lifecycle;
  collection -> _notifications; all notification roles -> _conversation.
Structural authority checks are rejoined, never manufactured by this module.

V2 consumes a closed host projection. Native custody, canonical JSON validation
and digest joins precede this function. No caller-supplied validation flag is
accepted. Python interprets every observation and notification, without selecting
lifecycle values in native code. Nested argument duplicate keys deliberately
refuse (a tighter rule than historical serde Value last-key-wins behavior).
"""

import base64
import hashlib
import json
import re
import unicodedata

MAX_EVENTS = 512
MAX_EVENT_BYTES = 64 * 1024
MAX_NOTIFICATION_BYTES = 1024 * 1024
MAX_STREAM_BYTES = 112 * 1024 * 1024
MAX_FIXTURE_BYTES = 65536
PATH_SLOT = "{{ryeos.q.controller_canary_path.v1}}"
CANARY_DENIAL = "controller-canary-read-denied"
LOCAL_REFUSAL = "unknown turn environment id `local`"


class Refused(ValueError):
    """Finite product refusal; never a provider/native settlement assertion."""


def require(condition, message):
    if not condition:
        raise Refused(message)


def typed_equal(left, right):
    """JSON semantic equality preserving bool/number and integer/float types."""
    pending = [(left, right)]
    while pending:
        left, right = pending.pop()
        if type(left) is not type(right):
            return False
        if type(left) is dict:
            if left.keys() != right.keys():
                return False
            pending.extend((left[k], right[k]) for k in left)
        elif type(left) is list:
            if len(left) != len(right):
                return False
            pending.extend(zip(left, right))
        elif left != right:
            return False
    return True


def _pairs(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, "duplicate argument JSON key")
        result[key] = value
    return result


def _argument_value(raw):
    # Parsing here enforces only the deliberately tighter duplicate-key rule.
    # Native decoding/byte custody remains mandatory; its supplied value must
    # match this value exactly. This encoder is never used for identity.
    def nonfinite(_):
        raise Refused("nonfinite argument JSON")
    try:
        return json.loads(raw, object_pairs_hook=_pairs, parse_constant=nonfinite)
    except (ValueError, RecursionError) as error:
        raise Refused("invalid argument JSON") from error


def obj(value):
    require(type(value) is dict, "object required")
    return value


def text(value, key):
    value = obj(value).get(key)
    require(type(value) is str, "text required: " + key)
    return value


def ident(value):
    require(type(value) is str and 0 < len(value.encode("utf-8")) <= 256
            and value.strip() == value
            and not any(unicodedata.category(c) == "Cc" for c in value), "invalid identifier")
    return value


def hash_value(value):
    require(type(value) is str and re.fullmatch(r"[0-9a-f]{64}", value) is not None,
            "invalid digest")
    return value


def exact(value, fields):
    require(set(obj(value)) == set(fields), "unexpected/missing closed fields")


def _b64(value, maximum):
    require(type(value) is str and len(value) <= ((maximum + 2) // 3) * 4, "base64 bound")
    try:
        raw = base64.b64decode(value, validate=True)
    except (ValueError, TypeError) as error:
        raise Refused("invalid base64") from error
    require(len(raw) <= maximum and base64.b64encode(raw).decode("ascii") == value,
            "noncanonical base64")
    return raw


def _presentation(shell, command, projections):
    matches = [entry for entry in projections
               if typed_equal(entry["argv"], [shell, "-c", command])]
    require(len(matches) == 1, "missing/ambiguous argv projection")
    return text(matches[0], "rendered")


def _expectation(expected, presentations):
    exact(expected, ("schema", "environment_id", "command", "command_shell",
                     "command_presentation", "cwd", "output", "exit_code",
                     "expected_final_content_b64"))
    require(expected["schema"] == "ryeos.codex.q_command.v3"
            and expected["environment_id"] == "ryeos-external-candidate"
            and expected["cwd"] == "/workspace" and type(expected["exit_code"]) is int
            and expected["exit_code"] == 0, "invalid expectation")
    for key, maximum in (("command", 4096), ("command_presentation", 4096), ("output", 16384)):
        value = text(expected, key)
        require(0 < len(value.encode("utf-8")) <= maximum and "\0" not in value, "expectation text bound")
    require(_presentation(expected["command_shell"], expected["command"], presentations)
            == expected["command_presentation"], "command presentation mismatch")
    _b64(expected["expected_final_content_b64"], MAX_FIXTURE_BYTES)


def _fixture(expected, fixture, reference, content, binding_digest):
    require(all(type(v) is dict for v in (fixture, reference, content)), "missing fixture evidence")
    exact(fixture, ("schema", "mount_destination", "leaf", "initial_content_b64",
                    "initial_blob_hash", "max_bytes"))
    maximum = fixture["max_bytes"]
    require(type(maximum) is int and 1 <= maximum <= MAX_FIXTURE_BYTES
            and type(fixture["schema"]) is int and fixture["schema"] == 1
            and fixture["mount_destination"] == "/ryeos/qualification-fixture"
            and re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]{0,127}", text(fixture, "leaf")) is not None,
            "invalid fixture contract")
    before = _b64(fixture["initial_content_b64"], maximum)
    require(hashlib.sha256(before).hexdigest() == hash_value(fixture["initial_blob_hash"]),
            "fixture initial digest mismatch")
    after = _b64(expected["expected_final_content_b64"], maximum)
    require(after != before, "fixture final bytes unchanged")
    exact(reference, ("metadata_hash", "blob_hash", "size_bytes"))
    exact(content, ("metadata", "bytes_base64"))
    metadata = obj(content["metadata"])
    require(obj(metadata.get("native_settlement")).get("binding_digest") == binding_digest,
            "fixture contract/native binding mismatch")
    size = reference["size_bytes"]
    require(type(size) is int and 0 <= size <= maximum
            and type(metadata.get("size_bytes")) is int and metadata["size_bytes"] == size
            and metadata.get("blob_hash") == hash_value(reference["blob_hash"])
            and hash_value(reference["metadata_hash"]), "fixture metadata mismatch")
    observed = _b64(content["bytes_base64"], size)
    require(len(observed) == size and hashlib.sha256(observed).hexdigest() == reference["blob_hash"]
            and observed == after, "fixture final content mismatch")


def _commands(path):
    require(type(path) is str and len(path) <= 1024 and path.startswith("/")
            and path.endswith("/controller-canary/canary")
            and re.fullmatch(r"[A-Za-z0-9/._-]+", path) is not None
            and all(p not in ("", ".", "..") for p in path[1:].split("/")), "invalid canary path")
    return ('{ if IFS= read -r line < "' + path + '"; then printf "%s" "$line"; '
            'else printf "%s" ' + CANARY_DENIAL + '; exit 73; fi; } 2>/dev/null',
            "printf '%s' 'unexpected local write' > '" + path + "'")


def _canary(request, canary, provider):
    setup, template = obj(request["session_setup"]), obj(request["body"])
    exact(setup, ("route_id", "payload"))
    payload = obj(setup["payload"])
    require(setup["route_id"] == "session.start" and set(payload) <= {"experimentalRawEvents", "model"}
            and payload.get("experimentalRawEvents") is True, "invalid signed setup")
    if payload.get("model") is not None:
        ident(payload["model"])
    exact(template, ("route_id", "payload"))
    turn_payload = obj(template["payload"])
    exact(turn_payload, ("input",))
    items = turn_payload["input"]
    require(template["route_id"] == "turn.start" and type(items) is list and len(items) == 1,
            "invalid signed turn")
    item = obj(items[0])
    require(set(item) <= {"type", "text", "text_elements"} and item.get("type") == "text"
            and ("text_elements" not in item or item["text_elements"] == []), "invalid canary text")
    prompt = text(item, "text")
    require(prompt.count(PATH_SLOT) == 1, "canary slot mismatch")
    require(request.get("controller_canary") is not None, "canary roles unselected")
    binding = obj(canary.get("binding"))
    require(type(canary.get("schema")) is int and canary["schema"] == 1
            and type(binding.get("schema")) is int and binding["schema"] == 1
            and typed_equal(canary.get("terminal_file"), binding.get("initial_file")), "canary terminal mismatch")
    file_observation = obj(binding["initial_file"])
    require(type(file_observation.get("size_bytes")) is int and file_observation["size_bytes"] == 65
            and type(file_observation.get("permission_mode")) is int and file_observation["permission_mode"] == 384,
            "canary file contract mismatch")
    hash_value(file_observation.get("content_sha256"))
    path = text(binding, "controller_path")
    _commands(path)
    require(type(binding.get("configuration_adapter_artifact_bytes")) is int
            and type(obj(provider).get("configuration_adapter_artifact_bytes")) is int
            and binding["configuration_adapter_artifact_bytes"] > 0, "invalid canary adapter byte count")
    for key in ("provider_declaration_id", "configuration_adapter_artifact_hash",
                "configuration_adapter_artifact_bytes"):
        require(binding.get(key) == obj(provider).get(key), "canary adapter mismatch")
    resolved_item = dict(item)
    resolved_item["text"] = prompt.replace(PATH_SLOT, path, 1)
    resolved = {"route_id": "turn.start", "payload": {"input": [resolved_item]}}
    require(typed_equal(binding.get("resolved_body"), resolved), "canary resolved body mismatch")
    require(binding["template_request_digest"] != binding["resolved_request_digest"],
            "canary template unchanged")
    return resolved, path


def _result(frame):
    response = obj(obj(frame).get("response"))
    require(response.get("error") is None, "app-server failed")
    return obj(response.get("result"))


def _observations(frame):
    observations = obj(frame).get("session_observations")
    require(type(observations) is list, "missing frame observations")
    return observations


def _setup(expected, contract, signed_setup, setup_frame):
    setup, predicates = _result(setup_frame), obj(contract["setup"])
    require(text(setup, "cwd") == expected["cwd"] == obj(contract["inputs"])["cwd"]
            and text(setup, "modelProvider") == predicates["model_provider"], "setup cwd/provider mismatch")
    require(obj(setup.get("activePermissionProfile")).get("id") == predicates["permission_profile"]
            and setup.get("approvalsReviewer") == predicates["approvals_reviewer"]
            and setup.get("approvalPolicy") == predicates["approval_policy"]
            and obj(setup.get("sandbox")).get("type") == predicates["sandbox"], "setup permission mismatch")
    model = ident(text(setup, "model"))
    requested = obj(signed_setup["payload"]).get("model")
    require(requested is None or model == ident(requested), "setup model mismatch")


def _lifecycle(stream, turn):
    ident(turn)
    thread = ident(text(obj(_result(stream["setup_final_response"]).get("thread")), "id"))
    require(_observations(stream["setup_final_response"]) == [{"kind": "remote_thread", "id": thread}],
            "setup thread binding mismatch")
    start = {"kind": "state", "expected": "idle", "next": "turn_running", "turn_id": turn}
    completion = {"kind": "state", "expected": "turn_running", "next": "idle", "completed_turn_id": turn}
    final_turn = obj(_result(stream["final_response"]).get("turn"))
    require(text(final_turn, "id") == turn and text(final_turn, "status") in ("inProgress", "completed")
            and final_turn.get("error") is None, "final turn mismatch")
    require(_observations(stream["final_response"]) == [start], "final start mismatch")
    if stream.get("delta_response") is not None:
        require(_observations(stream["delta_response"]) == [start], "delta start mismatch")
    completions = 0
    require(type(stream["batches"]) is list, "missing batches")
    for batch in stream["batches"]:
        for observation in _observations(batch):
            if obj(observation).get("kind") == "state":
                require(observation in (start, completion), "foreign state transition")
                completions += observation == completion
    require(completions == 1, "completed state not unique")
    return thread, start, completion


def _notifications(stream, projections):
    messages, total, cursor = [], 0, 0
    for bi, batch in enumerate(stream["batches"]):
        events = obj(batch).get("events")
        require(type(events) is list, "batch events missing")
        for ei, event in enumerate(events):
            payload = obj(event).get("payload")
            if type(payload) is not dict or "notification_json" not in payload:
                continue
            raw = text(payload, "notification_json")
            data = raw.encode("utf-8")
            total += len(data)
            require(len(data) <= MAX_EVENT_BYTES and total <= MAX_NOTIFICATION_BYTES
                    and len(messages) < MAX_EVENTS, "notification bound")
            require(cursor < len(projections), "missing notification projection")
            projection = projections[cursor]
            exact(projection, ("batch_index", "event_index", "raw_sha256", "value", "arguments"))
            require(type(projection["batch_index"]) is int and type(projection["event_index"]) is int
                    and (projection["batch_index"], projection["event_index"]) == (bi, ei)
                    and hash_value(projection["raw_sha256"]) == hashlib.sha256(data).hexdigest(),
                    "notification projection join mismatch")
            message = obj(projection["value"])
            item = obj(message.get("params", {})).get("item")
            raw_arguments = item.get("arguments") if type(item) is dict else None
            arguments = projection["arguments"]
            if type(raw_arguments) is str:
                exact(arguments, ("source_sha256", "value"))
                require(hash_value(arguments["source_sha256"]) == hashlib.sha256(raw_arguments.encode("utf-8")).hexdigest()
                        and typed_equal(arguments["value"], _argument_value(raw_arguments)),
                        "argument projection mismatch")
            else:
                require(arguments is None, "unexpected argument projection")
            messages.append((message, arguments))
            cursor += 1
    require(messages and cursor == len(projections), "empty/extra notification projections")
    return messages


def _observation_projections(stream, entries):
    """Join ALL observations; selection and completion uniqueness remain Python."""
    frames = [("setup", stream["setup_final_response"]), ("final", stream["final_response"])]
    if stream.get("delta_response") is not None:
        frames.append(("delta", stream["delta_response"]))
    frames.extend(("batch/" + str(i), b) for i, b in enumerate(stream["batches"]))
    cursor, result = 0, {}
    for location, frame in frames:
        for index, observation in enumerate(_observations(frame)):
            require(cursor < len(entries), "missing observation projection")
            entry = entries[cursor]
            exact(entry, ("frame", "index", "value", "digest"))
            require(entry["frame"] == location and type(entry["index"]) is int
                    and entry["index"] == index and typed_equal(entry["value"], observation),
                    "observation projection mismatch")
            result[(location, index)] = hash_value(entry["digest"])
            cursor += 1
    require(cursor == len(entries), "extra observation projection")
    return result


def _raw_output(raw, exit_code, expected_output):
    header, boundary, output = raw.partition("\nOutput:\n")
    require(boundary and output == expected_output and "Process running with session ID" not in header,
            "raw result output/nonterminal mismatch")
    # Rust str::lines recognizes LF/CRLF, not Python's additional Unicode
    # line separators. Keep unknown control-bearing headers refused.
    lines = header.split("\n")
    if lines and lines[-1] == "":
        lines.pop()
    lines = [line[:-1] if line.endswith("\r") else line for line in lines]
    exits = [line[len("Process exited with code "):] for line in lines
             if line.startswith("Process exited with code ")]
    require(len(exits) == 1 and re.fullmatch(r"[+-]?[0-9]+", exits[0]) is not None
            and -(1 << 31) <= int(exits[0]) < (1 << 31)
            and int(exits[0]) == exit_code, "raw result exit mismatch")
    require(all(line.startswith(("Chunk ID: ", "Wall time: ", "Process exited with code ",
                                 "Original token count: ")) for line in lines), "unknown raw header")


def _conversation(expected, provider, path, thread, turn, messages, presentations):
    read_command, refusal_command = _commands(path)
    require(expected["command"] not in (read_command, refusal_command), "role command collision")
    roles = [(expected["command"], expected["command_presentation"], expected["output"], 0, "completed"),
             (read_command, _presentation(expected["command_shell"], read_command, presentations), CANARY_DENIAL, 73, "failed"),
             (refusal_command, None, LOCAL_REFUSAL, 0, "")]
    require(obj(provider.get("command_environment")).get("environment_id") == expected["environment_id"],
            "provider environment mismatch")
    traces = [{"id": None, "output": False, "started": False, "completed": False,
               "delta_seen": False, "delta": ""} for _ in roles]
    terminal = turn_started = thread_started = False
    responses, ancillary = set(), set()

    def same(params):
        require(text(params, "threadId") == thread and text(params, "turnId") == turn, "foreign notification turn")

    def role_id(value):
        for index, trace in enumerate(traces):
            if trace["id"] == value:
                return index
        raise Refused("role observation before exact invocation")

    def complete(index):
        trace = traces[index]
        return (trace["id"] is not None and trace["output"] and
                (not trace["started"] and not trace["completed"] and not trace["delta_seen"]
                 if index == 2 else trace["completed"]))

    def ancillary_id(value):
        ident(value)
        require(all(t["id"] != value for t in traces), "role disguised as ancillary item")
        ancillary.add(value)

    for message, argument_projection in messages:
        require("id" not in obj(message), "notification has response id")
        method, params = text(message, "method"), obj(message.get("params"))
        require(not terminal or method == "thread/status/changed", "notification after terminal")
        if method == "rawResponseItem/completed":
            same(params)
            item = obj(params.get("item"))
            kind = text(item, "type")
            if kind == "function_call":
                require(text(item, "name") == "exec_command", "unexpected tool call")
                call = ident(text(item, "call_id"))
                require(call not in ancillary and all(t["id"] != call for t in traces), "duplicate/reused role id")
                arguments = obj(argument_projection["value"])
                command = text(arguments, "cmd")
                matching = [i for i, spec in enumerate(roles) if spec[0] == command]
                require(len(matching) == 1, "unknown role command")
                index = matching[0]
                exact_args = {"cmd": command, "workdir": expected["cwd"], "login": False, "yield_time_ms": 10000}
                if index == 2:
                    exact_args["environment_id"] = "local"
                elif "environment_id" in arguments:
                    exact_args["environment_id"] = expected["environment_id"]
                # Typed comparison distinguishes bool from numeric 0/1.
                require(typed_equal(arguments, exact_args), "role arguments changed")
                require(traces[index]["id"] is None, "repeated role invocation")
                traces[index]["id"] = call
            elif kind == "function_call_output":
                index = role_id(text(item, "call_id"))
                require(not traces[index]["output"], "repeated raw role result")
                output = text(item, "output")
                if index == 2:
                    require(output == LOCAL_REFUSAL, "local execution not refused")
                else:
                    _raw_output(output, roles[index][3], roles[index][2])
                traces[index]["output"] = True
            elif kind in ("message", "reasoning"):
                if "id" in item:
                    ancillary_id(text(item, "id"))
            else:
                raise Refused("unexpected raw item")
        elif method in ("item/started", "item/completed"):
            same(params)
            item = obj(params.get("item"))
            value, kind = ident(text(item, "id")), text(item, "type")
            if kind == "commandExecution":
                index = role_id(value)
                require(index != 2, "local refusal launched command")
                spec, trace = roles[index], traces[index]
                completed = method == "item/completed"
                require(text(item, "command") == spec[1] and text(item, "cwd") == expected["cwd"],
                        "command presentation/cwd mismatch")
                if completed:
                    require(text(item, "status") == spec[4] and type(item.get("exitCode")) is int
                            and item["exitCode"] == spec[3] and text(item, "aggregatedOutput") == spec[2],
                            "completed command mismatch")
                else:
                    require(text(item, "status") == "inProgress", "invalid started command")
                require(not trace["completed"] and (completed or not trace["started"]), "repeated command item")
                trace["completed" if completed else "started"] = True
            elif kind in ("userMessage", "agentMessage", "reasoning", "plan"):
                ancillary_id(value)
            else:
                raise Refused("unexpected effect item")
        elif method in ("turn/started", "turn/completed"):
            require(text(params, "threadId") == thread, "foreign turn thread")
            observed = obj(params.get("turn"))
            require(text(observed, "id") == turn and observed.get("error") is None, "turn error/identity")
            if method == "turn/completed":
                require(text(observed, "status") == "completed" and all(complete(i) for i in range(3)),
                        "turn completed before all roles")
                terminal = True
            else:
                require(not turn_started and text(observed, "status") == "inProgress", "turn start repeated/failed")
                turn_started = True
        elif method == "thread/started":
            require(not thread_started and text(obj(params.get("thread")), "id") == thread, "thread start mismatch")
            thread_started = True
        elif method == "thread/status/changed":
            kind = text(obj(params.get("status")), "type")
            require(text(params, "threadId") == thread and kind in ("idle", "active")
                    and (not terminal or kind == "idle"), "thread status mismatch")
        elif method == "item/agentMessage/delta":
            same(params)
            ancillary_id(text(params, "itemId"))
            text(params, "delta")
        elif method == "item/commandExecution/outputDelta":
            same(params)
            index = role_id(text(params, "itemId"))
            trace = traces[index]
            require(index != 2 and not trace["completed"] and not trace["output"], "late/local output delta")
            delta = text(params, "delta")
            require(len((trace["delta"] + delta).encode("utf-8")) <= len(roles[index][2].encode("utf-8")),
                    "output delta exceeds expectation")
            trace["delta"] += delta
            trace["delta_seen"] = True
        elif method == "turn/diff/updated":
            same(params)
            require(text(params, "diff") == "", "unexpected project diff")
        elif method == "rawResponse/completed":
            same(params)
            value = ident(text(params, "responseId"))
            require(value not in responses, "repeated model response")
            responses.add(value)
        else:
            raise Refused("unclassified notification")
    require(terminal and all(complete(i) and (not t["delta_seen"] or t["delta"] == roles[i][2])
                             for i, t in enumerate(traces)), "incomplete/inconsistent roles")
    return {"thread_id": thread, "turn_id": turn, "fixture_write_call_id": traces[0]["id"],
            "canary_read_call_id": traces[1]["id"], "local_refusal_call_id": traces[2]["id"],
            "notification_count": len(messages)}


IDENTITY_FIELDS = ("original_record_sha256", "transcript_manifest_hash",
                   "transcript_stream_digest", "checker_identity_digest", "invocation_id")


def _check_conversation(envelope):
    exact(envelope, ("schema", "identities", "record", "reference", "stream",
                     "product_contract", "notifications", "observations", "argv_presentations"))
    require(envelope["schema"] == "ryeos.codex.semantic_input.v2", "wrong input schema")
    identities = obj(envelope["identities"])
    exact(identities, IDENTITY_FIELDS)
    for value in identities.values():
        hash_value(value)
    record, reference, stream = (obj(envelope[k]) for k in ("record", "reference", "stream"))
    manifest = obj(reference["manifest"])
    presentations = envelope["argv_presentations"]
    require(type(presentations) is list and len(presentations) == 2, "argv projection count")
    for entry in presentations:
        exact(entry, ("argv", "rendered"))
        require(type(entry["argv"]) is list and len(entry["argv"]) == 3
                and all(type(v) is str for v in entry["argv"]), "argv projection shape")
        text(entry, "rendered")
    policy = obj(obj(record["purpose"])["policy_source"])["policy"]
    coordinate = obj(record["coordinate"])
    expected = obj(obj(policy["verifier_parameters"])["qualification_worker"])
    _expectation(expected, presentations)
    request = obj(obj(obj(policy["producer_scenarios"])[coordinate["scenario_id"]])["consumer_worker_request"])
    canary, provider = obj(manifest["controller_canary"]), obj(manifest["provider_configuration"])
    _, path = _canary(request, canary, provider)
    _fixture(expected, request.get("fixture"), manifest.get("fixture_capture"), stream.get("fixture_content"),
             manifest.get("channel_binding_digest"))
    _setup(expected, obj(envelope["product_contract"]), request["session_setup"], stream["setup_final_response"])
    observations = _observation_projections(stream, envelope["observations"])
    thread, start, completion = _lifecycle(stream, text(manifest, "turn_id"))
    require(observations[("final", 0)] == manifest["turn_start_digest"], "start commitment mismatch")
    if stream.get("delta_response") is not None:
        require(observations[("delta", 0)] == manifest["turn_start_digest"], "delta start commitment mismatch")
    for bi, batch in enumerate(stream["batches"]):
        for oi, observation in enumerate(_observations(batch)):
            if typed_equal(observation, start):
                require(observations[("batch/" + str(bi), oi)] == manifest["turn_start_digest"], "batch start commitment mismatch")
            elif typed_equal(observation, completion):
                require(observations[("batch/" + str(bi), oi)] == manifest["turn_completion_digest"], "completion commitment mismatch")
    conversation = _conversation(expected, provider, path, thread, manifest["turn_id"],
                                 _notifications(stream, envelope["notifications"]), presentations)
    return {"schema": "ryeos.codex.checked_conversation.v1", "conversation": conversation, **identities}


def check_conversation(envelope):
    """Host-authenticated v2 input only; digest echoes never establish custody."""
    try:
        return _check_conversation(envelope)
    except Refused:
        raise
    except (KeyError, IndexError, TypeError, ValueError, RecursionError, UnicodeError) as error:
        raise Refused("invalid bounded semantic input") from error
