#!/usr/bin/env python3
"""Strict response assertions for the explicit-coordinate live fixture."""

import hashlib
import json
import re
import sys


HASH = re.compile(r"^[0-9a-f]{64}$")


def fail(message: str) -> None:
    raise SystemExit(message)


def load(path: str):
    with open(path, encoding="utf-8") as source:
        return json.load(source)


def objects(value):
    if isinstance(value, dict):
        yield value
        for child in value.values():
            yield from objects(child)
    elif isinstance(value, list):
        for child in value:
            yield from objects(child)


def one_object(value, predicate, label: str):
    matches = {}
    for candidate in objects(value):
        if predicate(candidate):
            encoded = json.dumps(candidate, sort_keys=True, separators=(",", ":"))
            matches[encoded] = candidate
    if len(matches) != 1:
        fail(f"expected one distinct {label}, found {len(matches)}")
    return next(iter(matches.values()))


def require_hash(label: str, value) -> str:
    if not isinstance(value, str) or not HASH.fullmatch(value):
        fail(f"{label} is not a canonical SHA-256 digest")
    return value


def fixture_canonical_digest(value) -> str:
    # Fixture command results use only RyeOS canonical JSON's ASCII scalar
    # subset, so this encoding is byte-identical to the retained digest owner.
    encoded = json.dumps(
        value, ensure_ascii=True, sort_keys=True, separators=(",", ":")
    ).encode("utf-8")
    return hashlib.sha256(encoded).hexdigest()


def execution(value):
    terminal = one_object(
        value,
        lambda item: isinstance(item.get("thread_id"), str)
        and isinstance(item.get("chain_root_id"), str)
        and item.get("status") in {"completed", "failed", "continued"},
        "terminal execution envelope",
    )
    if terminal["status"] != "completed":
        fail(f"execution did not complete: {terminal['status']}")
    print(terminal["thread_id"])
    print(terminal["chain_root_id"])


def launched(value):
    launch = one_object(
        value,
        lambda item: isinstance(item.get("thread_id"), str)
        and item.get("status") in {"accepted", "running", "started"},
        "accepted asynchronous execution",
    )
    root = launch.get("chain_root_id", launch["thread_id"])
    if not isinstance(root, str):
        fail("accepted asynchronous execution has no canonical root coordinate")
    print(launch["thread_id"])
    print(root)


def thread(value, expected_thread: str, expected_root: str):
    detail = one_object(
        value,
        lambda item: item.get("thread_id") == expected_thread
        and item.get("chain_root_id") == expected_root
        and isinstance(item.get("status"), str),
        "exact thread detail",
    )
    if detail["status"] != "completed":
        fail(f"exact thread is not completed: {detail['status']}")
    if detail.get("successor_thread_id") is not None:
        fail("exact terminal thread has a continuation successor")


def terminal_disposition(value, expected_thread: str, expected_root: str):
    detail = one_object(
        value,
        lambda item: item.get("thread_id") == expected_thread
        and item.get("chain_root_id") == expected_root
        and isinstance(item.get("status"), str),
        "exact execution thread",
    )
    status = detail["status"]
    if status == "completed":
        thread(value, expected_thread, expected_root)
        print("completed")
    elif status in {"created", "running", "suspended"}:
        print("pending")
    else:
        fail(f"exact execution terminated without success: {status}")


def accepted_result(value):
    return one_object(
        value,
        lambda item: item.get("schema") == "ryeos.product_build_accepted_result.v1"
        and item.get("kind") == "product_build_accepted_result",
        "accepted product-build result",
    )


def accepted_equal(value, original_path: str):
    if accepted_result(value) != accepted_result(load(original_path)):
        fail("replay changed the complete accepted product-build result")


def accepted(value, expected_snapshot: str):
    result = accepted_result(value)
    if result.get("producer_ref") != "graph:test/two-products":
        fail("accepted result names the wrong producer")
    if result.get("producer_project_snapshot_hash") != expected_snapshot:
        fail("accepted producer generation differs from the requested pinned generation")
    require_hash("accepted producer definition", result.get("producer_effective_definition_digest"))
    require_hash("accepted producer parameters", result.get("producer_parameters_digest"))
    require_hash("accepted producer partition", result.get("producer_partition_identity"))
    products = result.get("products")
    if not isinstance(products, list) or [item.get("product_name") for item in products] != [
        "distribution",
        "runtime",
    ]:
        fail("accepted result does not contain the exact ordered fixture products")
    distribution, runtime = products
    if distribution.get("qualification_hash") is not None:
        fail("producer acceptance unexpectedly qualified the auxiliary distribution")
    if runtime.get("qualification_hash") is not None:
        fail("producer acceptance unexpectedly supplied independent qualification")
    print(require_hash("distribution witness", distribution.get("witness_hash")))
    print(require_hash("runtime witness", runtime.get("witness_hash")))


def node_identity(value):
    identity = one_object(
        value,
        lambda item: isinstance(item.get("principal_id"), str)
        and isinstance(item.get("fingerprint"), str)
        and isinstance(item.get("signing_key"), str),
        "node public identity",
    )
    fingerprint = require_hash("node public identity", identity["fingerprint"])
    print(fingerprint)


def origin_admission(value, expected_subject: str):
    admission = one_object(
        value,
        lambda item: item.get("subject_hash") == expected_subject
        and item.get("policy") == "local-node-v2"
        and item.get("claim") == "accepted",
        "origin product admission",
    )
    print(require_hash("origin admission", admission.get("attestation_hash")))


def received_product(
    value, expected_witness: str, expected_admission: str, expected_manifest: str
):
    received = one_object(
        value,
        lambda item: item.get("witness_hash") == expected_witness
        and item.get("origin_admission_hash") == expected_admission
        and item.get("manifest_hash") == expected_manifest,
        "received product acceptance",
    )
    if not str(received.get("job_id", "")).startswith("remote-import:"):
        fail("received product has no exact remote-import job coordinate")
    if not str(received.get("attempt_id", "")).startswith("remote-import-attempt:"):
        fail("received product has no exact remote-import attempt coordinate")
    print(require_hash("received product acceptance", received.get("acceptance_hash")))


def recorded_receipt(value, expected_source: str, expected_record: str | None = None):
    receipt = one_object(
        value,
        lambda item: item.get("node") == "produce"
        and isinstance(item.get("dispatch"), dict),
        "recorded producer node receipt",
    )
    dispatch = receipt["dispatch"]
    if dispatch.get("source") != expected_source:
        fail("recorded producer receipt has the wrong dispatch source")
    if dispatch.get("effect_class") != "recorded":
        fail("recorded producer receipt lost its recorded effect class")
    record_hash = require_hash("recorded producer effect record", dispatch.get("record_hash"))
    require_hash("recorded producer action", dispatch.get("action_digest"))
    require_hash("recorded producer effect identity", dispatch.get("effect_identity"))
    if expected_source == "executed":
        if dispatch.get("publication") not in {"inserted", "folded"}:
            fail("first recorded producer action did not publish its effect record")
        if dispatch.get("replayed_from") is not None:
            fail("first recorded producer action falsely claims replay provenance")
        print(record_hash)
        return
    if expected_source != "effect_record":
        fail("unsupported recorded receipt source assertion")
    if dispatch.get("publication") != "not_applicable":
        fail("replayed recorded producer action attempted a publication")
    if dispatch.get("replayed_from") != record_hash:
        fail("replayed producer receipt contradicts its retained record")
    if record_hash != expected_record:
        fail("replayed producer used a different effect record")


def replay_chain_leaf(value, expected_thread: str, expected_root: str):
    chain = one_object(
        value,
        lambda item: isinstance(item.get("threads"), list)
        and isinstance(item.get("edges"), list),
        "authoritative exact wrapper chain",
    )
    if chain["edges"]:
        fail("recorded replay birthed a child edge")
    threads = chain["threads"]
    if len(threads) != 1:
        fail(f"recorded replay birthed {len(threads) - 1} child threads")
    thread = threads[0]
    if thread.get("thread_id") != expected_thread or thread.get("chain_root_id") != expected_root:
        fail("recorded replay chain returned another exact root")


def retry_disposition(response_path: str, error_path: str, exit_status: str):
    try:
        status = int(exit_status)
    except ValueError:
        fail("retry disposition exit status is not numeric")
    if status == 124:
        print("retry")
        return
    try:
        value = load(response_path)
    except (OSError, UnicodeError, json.JSONDecodeError):
        value = None
    if value is not None and any(item.get("retryable") is True for item in objects(value)):
        print("retry")
        return
    messages = []
    if value is not None:
        for item in objects(value):
            for key in ("error", "message"):
                if isinstance(item.get(key), str):
                    messages.append(item[key])
    try:
        with open(error_path, encoding="utf-8", errors="replace") as source:
            messages.append(source.read())
    except OSError:
        pass
    diagnostic = "\n".join(messages).lower()
    transient = (
        "dedicated session has no attached worker",
        "connection refused",
        "connection reset",
        "failed to connect",
        "unexpected eof",
    )
    print("retry" if any(marker in diagnostic for marker in transient) else "permanent")


def imported(value, expected_manifest: str):
    response = one_object(
        value,
        lambda item: isinstance(item.get("staging_id"), str)
        and isinstance(item.get("request_digest"), str)
        and isinstance(item.get("manifest_hash"), str),
        "retained-product import response",
    )
    if response["manifest_hash"] != expected_manifest:
        fail("imported manifest differs from the deterministic fixture manifest")
    if response.get("manifest_kind") != "external_large_content_manifest":
        fail("runtime product did not use the declared large-content manifest")
    if response.get("entry_count") != 4 or response.get("total_bytes") != 9688:
        fail("runtime product metrics differ from the deterministic fixture")
    print(response["staging_id"])
    print(require_hash("import request digest", response["request_digest"]))


def bound(value, expected_manifest: str, expected_consumer: str = "tool:test/verify-runtime"):
    response = one_object(
        value,
        lambda item: isinstance(item.get("binding_hash"), str)
        and isinstance(item.get("manifest_hash"), str)
        and isinstance(item.get("consumer_ref"), str),
        "external-content binding response",
    )
    if response["manifest_hash"] != expected_manifest:
        fail("verifier binding names the wrong manifest")
    if response["consumer_ref"] != expected_consumer:
        fail("verifier binding names the wrong consumer")
    require_hash("verifier binding", response["binding_hash"])


def verifier_result(value, expected_manifest: str):
    result = one_object(
        value,
        lambda item: item.get("schema") == "ryeos.product_qualification_result.v1",
        "typed qualification result",
    )
    if result.get("subject_manifest_hash") != expected_manifest:
        fail("verifier result names the wrong subject manifest")
    if result.get("claims") != ["runtime_program_executed"]:
        fail("verifier result returned an unexpected claim set")
    if not isinstance(result.get("probe_evidence"), dict):
        fail("verifier result omitted bounded probe evidence")


def qualified(value):
    response = one_object(
        value,
        lambda item: isinstance(item.get("qualification_hash"), str)
        and isinstance(item.get("coordinate_id"), str)
        and isinstance(item.get("idempotent"), bool),
        "qualification publication response",
    )
    print(require_hash("qualification witness", response["qualification_hash"]))
    print(require_hash("qualification coordinate", response["coordinate_id"]))


def qualified_idempotent(value, expected_hash: str, expected_coordinate: str):
    response = one_object(
        value,
        lambda item: item.get("qualification_hash") == expected_hash
        and item.get("coordinate_id") == expected_coordinate
        and item.get("idempotent") is True,
        "idempotent qualification publication response",
    )
    print(require_hash("qualification witness", response["qualification_hash"]))
    print(require_hash("qualification coordinate", response["coordinate_id"]))


def profile_state(value, profile_id: str, expected_state: str):
    profile = one_object(
        value,
        lambda item: item.get("profile_id") == profile_id
        and item.get("state") == expected_state,
        f"credential profile in state {expected_state}",
    )
    if expected_state == "confirming":
        epoch = profile.get("login_epoch")
        if not isinstance(epoch, int) or isinstance(epoch, bool) or epoch <= 0:
            fail("observed credential profile has no positive login epoch")
        expected_digest = "16c0e6d7345067bf65c886ef80ecc7c6139bb61e06ff2989b35dc26488f9bb01"
        if profile.get("sanitized_account_digest") != expected_digest:
            fail("observed credential profile has the wrong exact account projection")
        print(epoch)
        print(expected_digest)


def confirmed_profile(value, profile_id: str):
    response = one_object(
        value,
        lambda item: item.get("profile_id") == profile_id
        and item.get("state") == "active"
        and isinstance(item.get("credential_generation"), int)
        and not isinstance(item.get("credential_generation"), bool),
        "exact confirmed credential profile",
    )
    generation = response["credential_generation"]
    if generation <= 0:
        fail("confirmed credential profile has no positive generation")
    digest = require_hash(
        "confirmed credential account", response.get("confirmed_account_digest")
    )
    print(generation)
    print(digest)


def active_profile(value, profile_id: str, expected_generation: str, expected_digest: str):
    try:
        generation = int(expected_generation)
    except ValueError:
        fail("expected credential generation is not numeric")
    digest = require_hash("expected credential account", expected_digest)
    profile = one_object(
        value,
        lambda item: item.get("profile_id") == profile_id
        and item.get("state") == "active"
        and item.get("credential_generation") == generation,
        "same active credential profile generation",
    )
    if profile.get("sanitized_account_digest") != digest:
        fail("active credential profile account differs from confirmed account")


def completion_fence(value):
    fence = one_object(
        value,
        lambda item: isinstance(item.get("placement_thread_id"), str)
        and isinstance(item.get("worker_boot_epoch"), int)
        and isinstance(item.get("command_sequence"), int)
        and isinstance(item.get("turn_id"), str)
        and isinstance(item.get("request_digest"), str)
        and isinstance(item.get("completion_operation_id"), str),
        "hosted command completion fence",
    )
    if fence["worker_boot_epoch"] <= 0 or fence["command_sequence"] <= 0:
        fail("hosted command completion fence has a non-positive sequence")
    require_hash("completion admitted capsule", fence.get("admitted_capsule_hash"))
    require_hash("completion request", fence["request_digest"])
    require_hash("completion operation", fence["completion_operation_id"])
    print(json.dumps(fence, sort_keys=True, separators=(",", ":")))


def credential_command(
    value,
    kind: str,
    expected_root: str,
    expected_placement: str,
    expected_sequence: str,
):
    if kind == "start":
        route_id = "credential.login.start"
        one_object(
            value,
            lambda item: item == {"login_id": "fixture-login-v1"},
            "offline enrollment-start result",
        )
    elif kind == "account":
        route_id = "credential.account.read"
        one_object(
            value,
            lambda item: item == {
                "account": {"email": "offline@example.test", "type": "fixture"}
            },
            "offline account-observation result",
        )
    else:
        fail("unknown credential command assertion")
    command = one_object(
        value,
        lambda item: item.get("state") == "completed"
        and isinstance(item.get("chain_root_id"), str)
        and isinstance(item.get("placement_thread_id"), str)
        and isinstance(item.get("command_sequence"), int)
        and not isinstance(item.get("command_sequence"), bool)
        and isinstance(item.get("result"), dict),
        "settled credential command",
    )
    if command["command_sequence"] <= 0:
        fail("settled credential command has a non-positive sequence")
    if (
        command["chain_root_id"] != expected_root
        or command["placement_thread_id"] != expected_placement
    ):
        fail("settled credential command names another launch coordinate")
    if str(command["command_sequence"]) != expected_sequence:
        fail("settled credential command has the wrong exact sequence")
    print(command["chain_root_id"])
    print(command["placement_thread_id"])
    print(command["command_sequence"])
    print(
        fixture_canonical_digest(
            {
                "command_kind": "route",
                "payload": {"route_id": route_id, "payload": {}},
            }
        )
    )
    print(fixture_canonical_digest(command["result"]))


def worker_result(
    value, expected_root: str, expected_placement: str, expected_sequence: str
):
    root = one_object(
        value,
        lambda item: item.get("chain_root_id") == expected_root
        and isinstance(item.get("placement_thread_id"), str),
        "exact worker command root",
    )
    if root["chain_root_id"] != expected_root:
        fail("worker command response names a different root")
    result = one_object(
        value,
        lambda item: item.get("schema") == "test.selected_runtime_execution.v1",
        "selected runtime execution result",
    )
    if result != {
        "schema": "test.selected_runtime_execution.v1",
        "marker": "selected-runtime-program-v1",
        "network_contacted": False,
    }:
        fail("selected runtime returned unexpected offline evidence")
    command = one_object(
        value,
        lambda item: item.get("chain_root_id") == expected_root
        and item.get("state") == "completed"
        and isinstance(item.get("placement_thread_id"), str)
        and isinstance(item.get("command_sequence"), int)
        and not isinstance(item.get("command_sequence"), bool)
        and isinstance(item.get("result"), dict),
        "settled selected-runtime command",
    )
    if command["command_sequence"] <= 0:
        fail("settled selected-runtime command has a non-positive sequence")
    if command["placement_thread_id"] != expected_placement:
        fail("settled selected-runtime command names another placement")
    if str(command["command_sequence"]) != expected_sequence:
        fail("settled selected-runtime command has the wrong exact sequence")
    print(command["chain_root_id"])
    print(command["placement_thread_id"])
    print(command["command_sequence"])
    print(
        fixture_canonical_digest(
            {
                "command_kind": "route",
                "payload": {"route_id": "session.run", "payload": {}},
            }
        )
    )
    print(fixture_canonical_digest(command["result"]))


def command_observation(
    value,
    expected_root: str,
    expected_placement: str,
    expected_sequence: str,
    expected_idempotency_key: str,
    expected_request_digest: str,
    expected_response_digest: str,
    expected_capsule_hash: str,
    expected_route_id: str,
    expected_operation: str,
):
    try:
        sequence = int(expected_sequence)
    except ValueError:
        fail("expected hosted command sequence is not numeric")
    request_digest = require_hash("expected command request", expected_request_digest)
    response_digest = require_hash("expected command response", expected_response_digest)
    capsule_hash = require_hash("expected session capsule", expected_capsule_hash)
    observation = one_object(
        value,
        lambda item: item.get("chain_root_id") == expected_root
        and item.get("placement_thread_id") == expected_placement
        and item.get("command_sequence") == sequence
        and item.get("command_state") == "completed",
        "exact settled hosted command observation",
    )
    if observation.get("admitted_capsule_hash") != capsule_hash:
        fail("authoritative command observation names another admitted session capsule")
    if observation.get("route_id") != expected_route_id:
        fail("authoritative command observation names another route")
    if observation.get("idempotency_key") != expected_idempotency_key:
        fail("authoritative command observation names another idempotency key")
    if observation.get("request_digest") != request_digest:
        fail("authoritative command observation names another request")
    if observation.get("response_digest") != response_digest:
        fail("authoritative command observation names another response")
    if expected_operation == "none":
        if observation.get("operation", "missing") is not None:
            fail("non-turn hosted command unexpectedly names a turn operation")
        if "completion_fence" in observation:
            fail("non-turn hosted command unexpectedly carries a completion fence")
        return
    if expected_operation != "completed-turn":
        fail("unknown hosted command operation assertion")
    operation = observation.get("operation")
    if not isinstance(operation, dict) or operation.get("kind") != "turn":
        fail("turn command observation has no exact turn operation")
    if operation.get("state") != "completed" or not isinstance(operation.get("id"), str):
        fail("turn command observation is not authoritatively completed")
    require_hash("turn start operation", operation.get("start_operation_id"))
    completion_operation = require_hash(
        "turn completion operation", operation.get("completion_operation_id")
    )
    fence = one_object(
        observation,
        lambda item: item.get("placement_thread_id") == expected_placement
        and item.get("command_sequence") == sequence
        and item.get("request_digest") == observation["request_digest"]
        and item.get("turn_id") == operation["id"]
        and item.get("completion_operation_id") == completion_operation,
        "exact hosted command completion fence",
    )
    if fence.get("worker_boot_epoch") != observation.get("worker_boot_epoch"):
        fail("hosted command completion fence changed worker boot epoch")
    if fence.get("admitted_capsule_hash") != observation.get("admitted_capsule_hash"):
        fail("hosted command completion fence changed admitted capsule")
    completion_fence(fence)


def worker_thread_authority(value, expected_thread: str, expected_root: str):
    thread = one_object(
        value,
        lambda item: item.get("thread_id") == expected_thread
        and item.get("chain_root_id") == expected_root
        and item.get("kind") == "worker_execution",
        "exact worker launch thread",
    )
    print(
        require_hash(
            "worker admitted launch capsule", thread.get("admitted_launch_capsule_hash")
        )
    )


def worker_thread_project_authority(
    value, expected_thread: str, expected_root: str, expected_snapshot: str
):
    snapshot = require_hash("expected worker project snapshot", expected_snapshot)
    thread = one_object(
        value,
        lambda item: item.get("thread_id") == expected_thread
        and item.get("chain_root_id") == expected_root
        and item.get("kind") == "worker_execution",
        "exact selected worker launch thread",
    )
    authority = thread.get("project_authority")
    if not isinstance(authority, dict) or authority.get("kind") != "pinned_generation":
        fail("selected worker did not retain pinned-generation project authority")
    if (
        authority.get("snapshot_hash") != snapshot
        or authority.get("base_snapshot_hash") != snapshot
    ):
        fail("selected worker launch admitted a different project generation")
    realization = authority.get("realization")
    if not isinstance(realization, dict) or realization.get("kind") != "cow":
        fail("selected worker did not retain its COW project realization")
    terminal = realization.get("terminal_publication")
    if not isinstance(terminal, dict) or terminal.get("kind") != "retain_current_head":
        fail("selected worker did not retain current-head terminal authority")
    if terminal.get("expected_hash") != snapshot:
        fail("selected worker terminal authority names another project generation")


def worker_session_authority(value, expected_root: str, expected_placement: str):
    session = one_object(
        value,
        lambda item: item.get("chain_root_id") == expected_root
        and item.get("placement_thread_id") == expected_placement
        and item.get("state") in {"idle", "turn_running"},
        "exact attached worker session",
    )
    print(require_hash("worker admitted session capsule", session.get("admitted_capsule_hash")))


def terminated(value, expected_root: str, expected_reason: str):
    response = one_object(
        value,
        lambda item: item.get("chain_root_id") == expected_root
        and item.get("state") == "terminal"
        and item.get("reason") == expected_reason,
        "exact worker termination",
    )
    if response["chain_root_id"] != expected_root:
        fail("worker termination names a different root")


def retained_termination(value, expected_root: str):
    response = one_object(
        value,
        lambda item: item.get("chain_root_id") == expected_root
        and item.get("reason") == "completed"
        and item.get("state") in {
            "freezing", "frozen", "verifying", "qualifying",
            "publish_ready", "terminal"
        },
        "completed retained worker termination",
    )
    if response["chain_root_id"] != expected_root:
        fail("retained worker termination names a different root")


def retained_candidate(value, expected_root: str):
    session = one_object(
        value,
        lambda item: item.get("chain_root_id") == expected_root
        and isinstance(item.get("candidate_snapshot_hash"), str)
        and item.get("publication_result") in {"retained", "retained_for_review"},
        "retained worker candidate",
    )
    print(require_hash("retained candidate snapshot", session["candidate_snapshot_hash"]))


def discarded(value, expected_root: str):
    one_object(
        value,
        lambda item: item.get("chain_root_id") == expected_root
        and item.get("discarded") is True,
        "discarded exact worker candidate",
    )


def composed(
    value,
    expected_snapshot: str,
    distribution_witness: str,
    runtime_witness: str,
    qualification: str,
    distribution_manifest: str,
    runtime_manifest: str,
    distribution_source=None,
    runtime_source=None,
):
    response = one_object(
        value,
        lambda item: isinstance(item.get("selections"), list)
        and isinstance(item.get("bindings"), list)
        and isinstance(item.get("consumer"), dict)
        and isinstance(item.get("pre_selection_effective_definition_digest"), str)
        and isinstance(item.get("selected_effective_definition_digest"), str),
        "product composition response",
    )
    if response.get("project_context") != {"snapshot_hash": expected_snapshot}:
        fail("composition changed the exact consumer generation")
    consumer = response["consumer"]
    if consumer.get("kind") != "pinned_project":
        fail("composition did not retain pinned-project consumer authority")
    if consumer.get("consumer_ref") != "config:test/runtime-consumer":
        fail("composition selected a different consumer")
    if consumer.get("project_snapshot_hash") != expected_snapshot:
        fail("composition consumer authority changed the exact generation")
    selections = response["selections"]
    distribution_source = distribution_source or {"kind": "local_capture"}
    runtime_source = runtime_source or {"kind": "local_capture"}
    if selections != [
        {
            "declaration_id": "distribution",
            "witness_hash": distribution_witness,
            "witness_source": distribution_source,
            "qualification_hash": None,
        },
        {
            "declaration_id": "runtime",
            "witness_hash": runtime_witness,
            "witness_source": runtime_source,
            "qualification_hash": qualification,
        },
    ]:
        fail("composition returned a different exact product selection batch")
    bindings = response["bindings"]
    if len(bindings) != 2:
        fail("composition did not return exactly two independent manifest bindings")
    expected_manifests = {
        "distribution": distribution_manifest,
        "runtime": runtime_manifest,
    }
    observed = {}
    for group in bindings:
        declarations = group.get("declaration_ids")
        if not isinstance(declarations, list) or len(declarations) != 1:
            fail("composition merged or omitted an independent fixture declaration")
        declaration = declarations[0]
        if declaration not in expected_manifests or declaration in observed:
            fail("composition returned an unknown or duplicate declaration binding")
        if group.get("manifest_kind") != "external_large_content_manifest":
            fail("composition returned a different manifest storage kind")
        if group.get("manifest_hash") != expected_manifests[declaration]:
            fail(f"composition bound the wrong {declaration} manifest")
        binding = group.get("binding")
        if not isinstance(binding, dict):
            fail(f"composition returned no ordinary {declaration} content binding")
        if binding.get("consumer_ref") != "config:test/runtime-consumer":
            fail(f"composition bound {declaration} to a different consumer")
        if binding.get("manifest_hash") != expected_manifests[declaration]:
            fail(f"ordinary {declaration} binding contradicts its manifest group")
        observed[declaration] = require_hash(
            f"{declaration} consumer binding", binding.get("binding_hash")
        )
    if set(observed) != set(expected_manifests):
        fail("composition did not bind the complete two-declaration batch")
    if observed["distribution"] == observed["runtime"]:
        fail("two distinct manifests unexpectedly shared one binding identity")
    pre_selection = require_hash(
        "pre-selection consumer definition",
        response["pre_selection_effective_definition_digest"],
    )
    selected = require_hash(
        "selected consumer definition", response["selected_effective_definition_digest"]
    )
    if selected == pre_selection:
        fail("complete two-product selection did not derive a distinct D1")
    print(observed["distribution"])
    print(observed["runtime"])


def received_composed(value, *expected):
    distribution_acceptance, runtime_acceptance = expected[6:]
    composed(
        value,
        *expected[:6],
        {"kind": "received", "acceptance_hash": distribution_acceptance},
        {"kind": "received", "acceptance_hash": runtime_acceptance},
    )
    response = one_object(
        value,
        lambda item: isinstance(item.get("selections"), list)
        and isinstance(item.get("bindings"), list)
        and isinstance(item.get("selected_effective_definition_digest"), str),
        "received product composition response",
    )
    print(require_hash("received pre-selection identity", response.get(
        "pre_selection_effective_definition_digest")))
    print(require_hash("received selected identity", response.get(
        "selected_effective_definition_digest")))


def resume_composition(
    value, expected_snapshot: str, distribution_manifest: str, runtime_manifest: str
):
    response = one_object(
        value,
        lambda item: isinstance(item.get("selections"), list)
        and isinstance(item.get("bindings"), list)
        and isinstance(item.get("consumer"), dict),
        "saved product composition response",
    )
    selections = response["selections"]
    if len(selections) != 2:
        fail("saved composition does not contain exactly two selections")
    distribution, runtime = selections
    if (
        distribution.get("declaration_id") != "distribution"
        or distribution.get("witness_source") != {"kind": "local_capture"}
        or distribution.get("qualification_hash") is not None
    ):
        fail("saved composition changed the unqualified distribution selection")
    if (runtime.get("declaration_id") != "runtime"
        or runtime.get("witness_source") != {"kind": "local_capture"}):
        fail("saved composition changed the qualified runtime selection")
    distribution_witness = require_hash(
        "saved distribution witness", distribution.get("witness_hash")
    )
    runtime_witness = require_hash("saved runtime witness", runtime.get("witness_hash"))
    qualification = require_hash(
        "saved runtime qualification", runtime.get("qualification_hash")
    )
    composed(
        value,
        expected_snapshot,
        distribution_witness,
        runtime_witness,
        qualification,
        distribution_manifest,
        runtime_manifest,
    )
    print(distribution_witness)
    print(runtime_witness)
    print(qualification)


def main():
    if len(sys.argv) < 3:
        fail("usage: assert-live-response.py <mode> <json-file> [expected values]")
    mode, path, *expected = sys.argv[1:]
    if mode == "retry-disposition" and len(expected) == 2:
        retry_disposition(path, *expected)
        return
    value = load(path)
    if mode == "execution" and not expected:
        execution(value)
    elif mode == "launch" and not expected:
        launched(value)
    elif mode == "thread" and len(expected) == 2:
        thread(value, *expected)
    elif mode == "terminal-disposition" and len(expected) == 2:
        terminal_disposition(value, *expected)
    elif mode == "accepted" and len(expected) == 1:
        accepted(value, *expected)
    elif mode == "accepted-equal" and len(expected) == 1:
        accepted_equal(value, *expected)
    elif mode == "node-identity" and not expected:
        node_identity(value)
    elif mode == "origin-admission" and len(expected) == 1:
        origin_admission(value, *expected)
    elif mode == "received-product" and len(expected) == 3:
        received_product(value, *expected)
    elif mode == "recorded-receipt" and len(expected) in {1, 2}:
        recorded_receipt(value, *expected)
    elif mode == "replay-chain-leaf" and len(expected) == 2:
        replay_chain_leaf(value, *expected)
    elif mode == "import" and len(expected) == 1:
        imported(value, *expected)
    elif mode == "bind" and len(expected) in {1, 2}:
        bound(value, *expected)
    elif mode == "verifier-result" and len(expected) == 1:
        verifier_result(value, *expected)
    elif mode == "qualification" and not expected:
        qualified(value)
    elif mode == "qualification-idempotent" and len(expected) == 2:
        qualified_idempotent(value, *expected)
    elif mode == "profile-state" and len(expected) == 2:
        profile_state(value, *expected)
    elif mode == "confirmed-profile" and len(expected) == 1:
        confirmed_profile(value, expected[0])
    elif mode == "active-profile" and len(expected) == 3:
        active_profile(value, *expected)
    elif mode == "credential-command" and len(expected) == 4:
        credential_command(value, *expected)
    elif mode == "command-observation" and len(expected) == 9:
        command_observation(value, *expected)
    elif mode == "completion-fence" and not expected:
        completion_fence(value)
    elif mode == "worker-result" and len(expected) == 3:
        worker_result(value, *expected)
    elif mode == "worker-thread-authority" and len(expected) == 2:
        worker_thread_authority(value, *expected)
    elif mode == "worker-thread-project-authority" and len(expected) == 3:
        worker_thread_project_authority(value, *expected)
    elif mode == "worker-session-authority" and len(expected) == 2:
        worker_session_authority(value, *expected)
    elif mode == "terminated" and len(expected) == 2:
        terminated(value, *expected)
    elif mode == "retained-termination" and len(expected) == 1:
        retained_termination(value, expected[0])
    elif mode == "retained-candidate" and len(expected) == 1:
        retained_candidate(value, expected[0])
    elif mode == "discarded" and len(expected) == 1:
        discarded(value, expected[0])
    elif mode == "composition" and len(expected) == 6:
        composed(value, *expected)
    elif mode == "received-composition" and len(expected) == 8:
        received_composed(value, *expected)
    elif mode == "resume-composition" and len(expected) == 3:
        resume_composition(value, *expected)
    else:
        fail(f"invalid arguments for response assertion mode {mode!r}")


if __name__ == "__main__":
    main()
