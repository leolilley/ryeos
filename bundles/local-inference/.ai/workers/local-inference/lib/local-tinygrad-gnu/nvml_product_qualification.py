"""ProductQualificationPolicy.v3 consumer for one raw NVIDIA assignment.

This module translates a retained signed verifier policy and D-injected raw
selection/provider into the existing NVML observer and product-result shape.
It performs no provider loading, selection, transport, funding, or admission.
"""
from __future__ import annotations

import re
from typing import Any


POLICY_SCHEMA = "ryeos.local_tinygrad_nvml_assignment_policy.v1"
RESULT_SCHEMA = "ryeos.product_qualification_result.v1"
EVIDENCE_SCHEMA = "ryeos.local_tinygrad_nvml_assignment_probe.v1"
CLAIM = "nvidia_nvml_primary_assignment_v1"
_POLICY_KEYS = frozenset(("schema", "primary_device_role", "l4_query_policy"))
_HASH = re.compile(r"[0-9a-f]{64}\Z")


class QualificationInputError(ValueError):
    pass


def _selected_query_policy(parameters: Any, observer_api: Any):
    if type(parameters) is not dict or set(parameters) != _POLICY_KEYS:
        raise QualificationInputError("NVML verifier parameters are not the signed finite policy")
    if parameters["schema"] != POLICY_SCHEMA:
        raise QualificationInputError("unsupported NVML verifier policy schema")
    role = parameters["primary_device_role"]
    if (type(role) is not str or not 1 <= len(role) <= 64
            or re.fullmatch(r"[A-Za-z0-9_.-]+", role) is None):
        raise QualificationInputError("signed primary-device role is unresolved or malformed")

    selected = parameters["l4_query_policy"]
    if selected is None:
        return role, None
    policy_type = getattr(observer_api, "L4QueryPolicy", None)
    if policy_type is None:
        raise QualificationInputError("selected NVML observer has no product-local query policy")
    try:
        policies = tuple(policy_type)
    except TypeError as error:
        raise QualificationInputError("selected NVML observer query policy is malformed") from error
    matches = [policy for policy in policies if getattr(policy, "value", None) == selected]
    if type(selected) is not str or len(matches) != 1:
        raise QualificationInputError("signed NVML query policy is not one exact observer profile")
    return role, matches[0]


def qualify_raw_assignment(
    *,
    subject_manifest_hash: str,
    verifier_parameters: Any,
    resource_selection: dict,
    bindings: Any,
) -> dict:
    """Return a bounded product candidate after the injected observer succeeds.

    `resource_selection` must come from D's retained raw selection, with no
    prefilled semantic facts. `bindings` must come from the exact selected
    verifier/provider closure. Its provenance is enforced by
    the accepted qualification owner, not by this copyable Python interface.
    """
    if not isinstance(subject_manifest_hash, str) or _HASH.fullmatch(subject_manifest_hash) is None:
        raise QualificationInputError("NVML qualification subject manifest is unresolved")
    # This import resolves only from the signed worker source closure. The
    # observer module itself accepts already-selected bindings and loads none.
    from cuda_lifecycle import nvml_observer

    role, query_policy = _selected_query_policy(verifier_parameters, nvml_observer)
    observation = nvml_observer.observe_assignment(
        request={
            "resource_selection": resource_selection,
            "primary_device_role": role,
        },
        bindings=bindings,
        l4_query_policy=query_policy,
    )
    if type(observation) is not dict or set(observation) != {
        "version", "stable_resource_id", "device_binding_digest",
        "primary_device", "uuid_v2_hex", "nvml_shutdown",
    }:
        raise QualificationInputError("NVML observer returned an unexpected candidate shape")
    primary_device = observation["primary_device"]
    if (type(primary_device) is not dict
            or observation["version"] != 1
            or observation["stable_resource_id"] != resource_selection.get("stable_id")
            or observation["device_binding_digest"] != resource_selection.get("device_binding_digest")
            or primary_device.get("role") != role
            or observation["nvml_shutdown"] != "closed"
            or type(observation["uuid_v2_hex"]) is not str
            or re.fullmatch(r"[0-9a-f]{32}", observation["uuid_v2_hex"]) is None
            or observation["uuid_v2_hex"] == "0" * 32):
        raise QualificationInputError("NVML candidate does not match the retained raw assignment")

    return {
        "schema": RESULT_SCHEMA,
        "subject_manifest_hash": subject_manifest_hash,
        "claims": [CLAIM],
        "probe_evidence": {
            "schema": EVIDENCE_SCHEMA,
            "subject_manifest_hash": subject_manifest_hash,
            "verifier_policy": dict(verifier_parameters),
            "assignment": observation,
        },
    }
