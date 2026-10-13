"""Authored conformance/mutation cases; synthetic input grants no authority.

Execution is deferred. Intended invocation after allocation: load checker as
ordinary selected test source, then run this unittest module. No live provider,
filesystem evidence or process/scope qualification is constructed by fixtures.
"""

import base64
import copy
import hashlib
import json
import shlex
import unittest

import checker


# Synthetic test-only identity construction is NOT a Rust canonical oracle.
# Real byte/canonical/projection conformance is a separately authorized host gate.
def synthetic_json(value):
    return json.dumps(value, sort_keys=True, ensure_ascii=True, separators=(",", ":"))


def synthetic_digest(value):
    return hashlib.sha256(synthetic_json(value).encode("ascii")).hexdigest()


def synthetic_decode(raw, maximum):
    return json.loads(raw)


def synthetic_presentation(shell, command):
    return shlex.join([shell, "-c", command])


def projected(values):
    record, reference, stream = values
    policy = record["purpose"]["policy_source"]["policy"]
    expected = policy["verifier_parameters"]["qualification_worker"]
    path = reference["manifest"]["controller_canary"]["binding"]["controller_path"]
    notifications, observations = [], []
    frames = [("setup", stream["setup_final_response"]), ("final", stream["final_response"])]
    if stream["delta_response"] is not None:
        frames.append(("delta", stream["delta_response"]))
    frames.extend(("batch/" + str(i), b) for i, b in enumerate(stream["batches"]))
    for location, frame in frames:
        for index, value in enumerate(frame["session_observations"]):
            observations.append({"frame": location, "index": index, "value": copy.deepcopy(value),
                                 "digest": synthetic_digest(value)})
    for bi, batch in enumerate(stream["batches"]):
        for ei, event in enumerate(batch["events"]):
            raw = event.get("payload", {}).get("notification_json")
            if raw is None:
                continue
            value = json.loads(raw)
            arguments = value.get("params", {}).get("item", {}).get("arguments")
            notifications.append({"batch_index": bi, "event_index": ei,
                "raw_sha256": hashlib.sha256(raw.encode("utf-8")).hexdigest(), "value": value,
                "arguments": None if not isinstance(arguments, str) else {
                    "source_sha256": hashlib.sha256(arguments.encode("utf-8")).hexdigest(),
                    "value": json.loads(arguments)}})
    commands = [expected["command"], checker._commands(path)[0]]
    return {"schema": "ryeos.codex.semantic_input.v2", "record": record, "reference": reference,
            "stream": stream, "product_contract": json.loads(record["scenario_input"]["product_contract_json"]),
            "identities": {"original_record_sha256": synthetic_digest(record),
                "transcript_manifest_hash": reference["manifest_hash"], "transcript_stream_digest": synthetic_digest(stream),
                "checker_identity_digest": "1" * 64, "invocation_id": "2" * 64},
            "notifications": notifications, "observations": observations,
            "argv_presentations": [{"argv": [expected["command_shell"], "-c", cmd],
                                    "rendered": synthetic_presentation(expected["command_shell"], cmd)} for cmd in commands]}


def b64(value):
    return base64.b64encode(value).decode("ascii")


def notification(method, **extra):
    return {"method": method, "params": {"threadId": "thread-one", "turnId": "turn-one", **extra}}


def fixture():
    path = "/private/Q-test/controller-canary/canary"
    template = {"route_id": "turn.start", "payload": {"input": [{"type": "text", "text": checker.PATH_SLOT}]}}
    resolved = {"route_id": "turn.start", "payload": {"input": [{"type": "text", "text": path}]}}
    provider = {"provider_declaration_id": "openai", "configuration_adapter_artifact_hash": "a" * 64,
                "configuration_adapter_artifact_bytes": 1,
                "command_environment": {"environment_id": "ryeos-external-candidate"}}
    file_observation = {"size_bytes": 65, "permission_mode": 384, "content_sha256": "b" * 64}
    binding_input = {"schema": 1, "template": template, "controller_canary_path": path}
    binding_output = {"schema": 1, "input_digest": synthetic_digest(binding_input), "resolved_body": resolved}
    canary = {"schema": 1, "terminal_file": file_observation,
              "binding": {"schema": 1, "controller_path": path, "initial_file": file_observation,
                          **{key: provider[key] for key in ("provider_declaration_id",
                             "configuration_adapter_artifact_hash", "configuration_adapter_artifact_bytes")},
                          "binding_input_digest": synthetic_digest(binding_input),
                          "binding_output_digest": synthetic_digest(binding_output),
                          "template_request_digest": synthetic_digest(template),
                          "resolved_request_digest": synthetic_digest(resolved), "resolved_body": resolved}}
    expected = {"schema": "ryeos.codex.q_command.v3", "environment_id": "ryeos-external-candidate",
                "command": "printf probe", "command_shell": "zsh",
                "command_presentation": "zsh -c 'printf probe'", "cwd": "/workspace",
                "output": "probe", "exit_code": 0, "expected_final_content_b64": b64(b"after")}
    contract = {"inputs": {"cwd": "/workspace"},
                "setup": {"model_provider": "openai", "permission_profile": "pinned",
                          "approvals_reviewer": "user", "approval_policy": "never", "sandbox": "external"}}
    signed_fixture = {"schema": 1, "mount_destination": "/ryeos/qualification-fixture", "leaf": "result",
                      "initial_content_b64": b64(b"before"),
                      "initial_blob_hash": hashlib.sha256(b"before").hexdigest(), "max_bytes": 64}
    metadata = {"kind": "synthetic_fixture_metadata", "schema": 1,
                "fixture_contract_digest": synthetic_digest(signed_fixture),
                "native_settlement": {"binding_digest": "c" * 64},
                "blob_hash": hashlib.sha256(b"after").hexdigest(), "size_bytes": 5}
    capture_reference = {"metadata_hash": synthetic_digest(metadata), "blob_hash": metadata["blob_hash"], "size_bytes": 5}
    request = {"session_setup": {"route_id": "session.start", "payload": {"experimentalRawEvents": True}},
               "body": template, "controller_canary": "original_controller_read_and_local_refusal_v1",
               "fixture": signed_fixture}
    record = {"coordinate": {"accepted_root_id": "T-root", "accepted_capsule_hash": "d" * 64, "scenario_id": "remote_codex"},
              "purpose": {"owner_fingerprint": "e" * 64, "policy_source": {"policy": {
                  "verifier_parameters": {"controller": {"scenario_id": "remote_codex"}, "qualification_worker": expected},
                  "producer_scenarios": {"remote_codex": {"consumer_worker_request": request}}}}},
              "scenario_input": {"product_contract_json": synthetic_json(contract)}}
    setup = {"response": {"result": {"thread": {"id": "thread-one"}, "cwd": "/workspace",
                                     "modelProvider": "openai", "model": "model-one",
                                     "activePermissionProfile": {"id": "pinned"}, "approvalsReviewer": "user",
                                     "approvalPolicy": "never", "sandbox": {"type": "external"}}},
             "session_observations": [{"kind": "remote_thread", "id": "thread-one"}]}
    start = {"kind": "state", "expected": "idle", "next": "turn_running", "turn_id": "turn-one"}
    completion = {"kind": "state", "expected": "turn_running", "next": "idle", "completed_turn_id": "turn-one"}
    final = {"response": {"result": {"turn": {"id": "turn-one", "status": "completed"}}},
             "session_observations": [start]}
    messages = [notification("turn/started", turn={"id": "turn-one", "status": "inProgress"})]
    commands = [expected["command"], *checker._commands(path)]
    for index, command in enumerate(commands):
        call = "call-" + str(index)
        args = {"cmd": command, "workdir": "/workspace", "login": False, "yield_time_ms": 10000}
        if index == 2:
            args["environment_id"] = "local"
        messages.append(notification("rawResponseItem/completed", item={"type": "function_call", "name": "exec_command",
                                      "call_id": call, "arguments": synthetic_json(args)}))
        if index != 2:
            output, status, code = (("probe", "completed", 0) if index == 0 else (checker.CANARY_DENIAL, "failed", 73))
            item = {"type": "commandExecution", "id": call, "command": synthetic_presentation("zsh", command),
                    "cwd": "/workspace", "status": status, "exitCode": code, "aggregatedOutput": output}
            messages.append(notification("item/completed", item=item))
            raw = "Process exited with code " + str(code) + "\nOutput:\n" + output
        else:
            raw = checker.LOCAL_REFUSAL
        messages.append(notification("rawResponseItem/completed", item={"type": "function_call_output", "call_id": call, "output": raw}))
    messages.append(notification("turn/completed", turn={"id": "turn-one", "status": "completed"}))
    batch = {"session_observations": [completion],
             "events": [{"payload": {"notification_json": synthetic_json(message)}} for message in messages]}
    stream = {"setup_final_response": setup, "final_response": final, "delta_response": None,
              "fixture_content": {"metadata": metadata, "bytes_base64": b64(b"after")}, "batches": [batch]}
    manifest = {"accepted_root_id": "T-root", "accepted_capsule_hash": "d" * 64, "owner_principal": "e" * 64,
                "controller_canary": canary, "provider_configuration": provider,
                "request_digest": synthetic_digest(resolved), "fixture_capture": capture_reference,
                "channel_binding_digest": "c" * 64, "setup_final_response_digest": synthetic_digest(setup),
                "final_response_digest": synthetic_digest(final), "delta_digest": None, "turn_id": "turn-one",
                "turn_start_digest": synthetic_digest(start), "turn_completion_digest": synthetic_digest(completion)}
    return record, {"manifest_hash": "f" * 64, "manifest": manifest}, stream


class CheckerSourceTests(unittest.TestCase):
    def check(self, values):
        return checker.check_conversation(projected(values))

    def test_three_role_positive_and_digest_bindings(self):
        values = fixture()
        result = self.check(values)
        self.assertEqual(result["conversation"]["fixture_write_call_id"], "call-0")
        self.assertEqual(result["conversation"]["canary_read_call_id"], "call-1")
        self.assertEqual(result["conversation"]["local_refusal_call_id"], "call-2")
        self.assertEqual(result["original_record_sha256"], synthetic_digest(values[0]))
        self.assertEqual(result["transcript_stream_digest"], synthetic_digest(values[2]))

    def test_changed_setup_model_permissions_fixture_and_canary_refuse(self):
        mutations = (
            lambda r, m, s: s["setup_final_response"]["response"]["result"].update(cwd="/elsewhere"),
            lambda r, m, s: s["fixture_content"].update(bytes_base64=b64(b"wrong")),
            lambda r, m, s: m["manifest"]["controller_canary"]["binding"].update(controller_path="/unsafe"),
            lambda r, m, s: s["final_response"]["response"]["result"]["turn"].update(status="failed"),
        )
        for mutation in mutations:
            values = fixture()
            mutation(*values)
            with self.subTest(mutation=mutation), self.assertRaises(checker.Refused):
                self.check(values)

    def test_effect_route_duplicate_terminal_and_local_execution_refuse(self):
        for mutation in ("foreign_turn", "repeat_call", "extra_effect", "local_started", "postterminal", "wrong_delta"):
            values = fixture()
            batch = values[2]["batches"][0]
            messages = [synthetic_decode(e["payload"]["notification_json"], checker.MAX_EVENT_BYTES) for e in batch["events"]]
            if mutation == "foreign_turn":
                messages[1]["params"]["turnId"] = "foreign"
            elif mutation == "repeat_call":
                messages.insert(2, copy.deepcopy(messages[1]))
            elif mutation == "extra_effect":
                messages.insert(2, notification("item/completed", item={"type": "fileChange", "id": "unknown"}))
            elif mutation == "local_started":
                messages.insert(-1, notification("item/started", item={"type": "commandExecution", "id": "call-2"}))
            elif mutation == "postterminal":
                messages.append(notification("turn/diff/updated", diff=""))
            else:
                messages.insert(2, notification("item/commandExecution/outputDelta", itemId="call-0", delta="wrong"))
            batch["events"] = [{"payload": {"notification_json": synthetic_json(m)}} for m in messages]
            with self.subTest(mutation=mutation), self.assertRaises(checker.Refused):
                self.check(values)

    def test_projection_join_and_closed_envelope_refuse(self):
        for mutation in ("missing", "extra", "reorder", "digest", "observation", "presentation", "identity"):
            envelope = projected(fixture())
            if mutation == "missing":
                envelope["notifications"].pop()
            elif mutation == "extra":
                envelope["observations"].append(copy.deepcopy(envelope["observations"][-1]))
            elif mutation == "reorder":
                envelope["notifications"].reverse()
            elif mutation == "digest":
                envelope["notifications"][0]["raw_sha256"] = "0" * 64
            elif mutation == "observation":
                envelope["observations"][-1]["value"]["kind"] = "foreign"
            elif mutation == "presentation":
                envelope["argv_presentations"][0]["argv"][0] = "bash"
            else:
                envelope["identities"]["caller_validated"] = True
            with self.subTest(mutation=mutation), self.assertRaises(checker.Refused):
                checker.check_conversation(envelope)

    def test_duplicate_argument_keys_deliberately_refuse_both_last_values(self):
        for last_value in (False, True):
            values = fixture()
            batch = values[2]["batches"][0]
            call = json.loads(batch["events"][1]["payload"]["notification_json"])
            args = json.loads(call["params"]["item"]["arguments"])
            raw = synthetic_json(args)
            # Last false reproduces the old serde-accepted command; both cases
            # now deliberately refuse. The nested duplicate stays raw in source.
            raw = raw[:-1] + ',"login":' + ('true' if last_value else 'false') + '}'
            call["params"]["item"]["arguments"] = raw
            batch["events"][1]["payload"]["notification_json"] = synthetic_json(call)
            with self.subTest(last_value=last_value), self.assertRaises(checker.Refused):
                self.check(values)

    def test_boolean_numeric_arguments_are_distinct(self):
        values = fixture()
        batch = values[2]["batches"][0]
        call = json.loads(batch["events"][1]["payload"]["notification_json"])
        args = json.loads(call["params"]["item"]["arguments"])
        args["login"] = 0
        call["params"]["item"]["arguments"] = synthetic_json(args)
        batch["events"][1]["payload"]["notification_json"] = synthetic_json(call)
        with self.assertRaises(checker.Refused):
            self.check(values)

    def test_opaque_float_and_deep_metadata_does_not_reencode_or_refuse(self):
        values = fixture()
        deep = 1.25
        for _ in range(80):
            deep = [deep]
        values[0]["opaque"] = deep
        values[2]["setup_final_response"]["opaque"] = {"float": -0.0, "deep": deep}
        result = self.check(values)
        self.assertEqual(result["conversation"]["fixture_write_call_id"], "call-0")

    def test_all_observations_are_projected_without_native_lifecycle_selection(self):
        values = fixture()
        values[2]["batches"][0]["session_observations"].insert(0, {"kind": "opaque", "float": 1.5})
        envelope = projected(values)
        self.assertEqual(len(envelope["observations"]), 4)
        self.assertEqual(checker.check_conversation(envelope)["conversation"]["turn_id"], "turn-one")
        envelope["observations"].pop(2)
        with self.assertRaises(checker.Refused):
            checker.check_conversation(envelope)

    def test_numeric_schema_and_canary_boolean_mutations_refuse(self):
        for field in ("fixture_schema", "canary_schema", "binding_schema", "size", "mode", "adapter_bytes", "capture_size"):
            values = fixture()
            policy = values[0]["purpose"]["policy_source"]["policy"]
            request = policy["producer_scenarios"]["remote_codex"]["consumer_worker_request"]
            manifest = values[1]["manifest"]
            canary = manifest["controller_canary"]
            if field == "fixture_schema":
                request["fixture"]["schema"] = True
            elif field == "canary_schema":
                canary["schema"] = True
            elif field == "binding_schema":
                canary["binding"]["schema"] = True
            elif field in ("size", "mode"):
                key = "size_bytes" if field == "size" else "permission_mode"
                canary["binding"]["initial_file"][key] = True
            elif field == "adapter_bytes":
                canary["binding"]["configuration_adapter_artifact_bytes"] = True
                manifest["provider_configuration"]["configuration_adapter_artifact_bytes"] = True
            else:
                manifest["fixture_capture"]["size_bytes"] = True
                values[2]["fixture_content"]["metadata"]["size_bytes"] = True
            with self.subTest(field=field), self.assertRaises(checker.Refused):
                self.check(values)

    def test_optional_delta_positive_and_observation_digest_join(self):
        values = fixture()
        values[2]["delta_response"] = copy.deepcopy(values[2]["final_response"])
        values[1]["manifest"]["delta_digest"] = synthetic_digest(values[2]["delta_response"])
        envelope = projected(values)
        self.assertEqual(envelope["observations"][2]["frame"], "delta")
        self.assertEqual(checker.check_conversation(envelope)["conversation"]["turn_id"], "turn-one")
        envelope["observations"][2]["digest"] = "0" * 64
        with self.assertRaises(checker.Refused):
            checker.check_conversation(envelope)
