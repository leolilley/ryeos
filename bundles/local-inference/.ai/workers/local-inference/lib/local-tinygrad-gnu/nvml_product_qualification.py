"""Bounded preflight adapter for one NVIDIA assignment observation.

This module consumes a retained finite verifier policy and D-injected raw
selection/provider, then returns the existing strict observer DTO. It performs
no provider loading, selection, transport, funding, or admission.
"""
from __future__ import annotations

import re
from copy import deepcopy
from typing import Any


POLICY_SCHEMA = "ryeos.local_tinygrad_nvml_assignment_policy.v1"
_POLICY_KEYS = frozenset(("schema", "primary_device_role", "l4_query_policy"))
_DEVICE_KEYS = frozenset(("role", "destination", "access", "major", "minor"))
_OBSERVATION_KEYS = frozenset(("version", "stable_resource_id", "device_binding_digest",
                               "primary_device", "uuid_v2_hex", "nvml_shutdown"))
_TOKEN = re.compile(r"[A-Za-z0-9_.-]+\Z")
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


def observe_preflight_candidate(
    verifier_parameters: Any,
    resource_selection: dict,
    bindings: Any,
) -> dict:
    """Return the strict existing observer DTO for one retained assignment.

    `resource_selection` must come from D's retained raw selection, with no
    prefilled semantic facts. `bindings` must come from the exact selected
    verifier/provider closure. This function does not establish that provenance;
    production admission enforces it outside this copyable Python interface.
    """
    if type(resource_selection) is not dict:
        raise QualificationInputError("NVML preflight selection is malformed")
    # This import resolves only from the signed worker source closure. The
    # observer module itself accepts already-selected bindings and loads none.
    from cuda_lifecycle import nvml_observer

    role, query_policy = _selected_query_policy(verifier_parameters, nvml_observer)
    # Give the observer a detached DTO and retain only immutable comparison
    # values before native calls. A mutable caller cannot rewrite the expected
    # assignment while observation is in progress.
    selection = deepcopy(resource_selection)
    stable_id = selection.get("stable_id")
    binding_digest = selection.get("device_binding_digest")
    devices = selection.get("character_devices")
    primary = None
    if type(devices) is list:
        for device in devices:
            if type(device) is dict and device.get("role") == role:
                primary = dict(device)
                break
    if primary is None:
        raise QualificationInputError("NVML preflight primary assignment is unresolved")

    observation = nvml_observer.observe_assignment(
        request={
            "resource_selection": selection,
            "primary_device_role": role,
        },
        bindings=bindings,
        l4_query_policy=query_policy,
    )
    if type(observation) is not dict or set(observation) != _OBSERVATION_KEYS:
        raise QualificationInputError("NVML observer returned an unexpected candidate shape")
    primary_device = observation["primary_device"]
    if (type(observation["version"]) is not int or observation["version"] != 1
            or type(stable_id) is not str or _TOKEN.fullmatch(stable_id) is None
            or type(binding_digest) is not str or _HASH.fullmatch(binding_digest) is None
            or type(observation["stable_resource_id"]) is not str
            or observation["stable_resource_id"] != stable_id
            or type(observation["device_binding_digest"]) is not str
            or observation["device_binding_digest"] != binding_digest
            or type(primary_device) is not dict or set(primary_device) != _DEVICE_KEYS
            or primary_device != primary or primary_device.get("role") != role
            or type(primary_device.get("destination")) is not str
            or re.fullmatch(r"/dev/nvidia(?:0|[1-9][0-9]*)", primary_device["destination"]) is None
            or primary_device.get("access") != "read_write"
            or type(primary_device.get("major")) is not int or primary_device["major"] != 195
            or type(primary_device.get("minor")) is not int
            or not 0 <= primary_device["minor"] < 254
            or primary_device["destination"] != f'/dev/nvidia{primary_device["minor"]}'
            or type(observation["nvml_shutdown"]) is not str
            or observation["nvml_shutdown"] != "closed"
            or type(observation["uuid_v2_hex"]) is not str
            or re.fullmatch(r"[0-9a-f]{32}", observation["uuid_v2_hex"]) is None
            or observation["uuid_v2_hex"] == "0" * 32):
        raise QualificationInputError("NVML candidate does not match the retained raw assignment")
    return observation
