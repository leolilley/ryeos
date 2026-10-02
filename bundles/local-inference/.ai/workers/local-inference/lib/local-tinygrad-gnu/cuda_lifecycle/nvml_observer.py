"""Finite NVIDIA primary-device observation for an existing raw qualification.

Provider injection only: no import/discovery/dlopen, descriptor access, clock,
process or transport. D owns retained assignment/provider/occurrence/funding.
The returned DTO is a comparison candidate, never admitted facts or cleanup.
No native provider or occurrence qualification is currently selected.
"""
from __future__ import annotations

import ctypes
import re


class NvmlObservationError(RuntimeError):
    pass


_SELECTION_KEYS = frozenset(("stable_id", "class", "matched_facts",
    "observation_contract_digest", "device_binding_digest", "access",
    "enforcement", "character_devices"))
_DEVICE_KEYS = frozenset(("role", "destination", "access", "major", "minor"))
_UINT_UNKNOWN = 0xffffffff
_PRIMARY_MAJOR = 195  # selected Linux NVIDIA contract; native nvproxy qualification remains required
_UUID_BYTES = 96
_UINT_PTR = ctypes.POINTER(ctypes.c_uint)
_HANDLE = ctypes.c_void_p
_PROTOTYPES = {
    "nvmlInit_v2": (),
    "nvmlDeviceGetCount_v2": (_UINT_PTR,),
    "nvmlDeviceGetHandleByIndex_v2": (ctypes.c_uint, ctypes.POINTER(_HANDLE)),
    "nvmlDeviceIsMigDeviceHandle": (_HANDLE, _UINT_PTR),
    "nvmlDeviceGetMigMode": (_HANDLE, _UINT_PTR, _UINT_PTR),
    "nvmlDeviceGetMinorNumber": (_HANDLE, _UINT_PTR),
    "nvmlDeviceGetUUID": (_HANDLE, ctypes.POINTER(ctypes.c_char), ctypes.c_uint),
    "nvmlShutdown": (),
}


def _token(value, limit):
    return (isinstance(value, str) and 1 <= len(value) <= limit
            and re.fullmatch(r"[A-Za-z0-9_.-]+", value) is not None)


def _assignment(request):
    # A strict product subset of the EXISTING selection DTO, not a core parser.
    if type(request) is not dict or set(request) != {"resource_selection", "primary_device_role"}:
        raise NvmlObservationError("expected one whole raw selection and primary role")
    selection, role = request["resource_selection"], request["primary_device_role"]
    if (type(selection) is not dict or set(selection) != _SELECTION_KEYS
            or not _token(role, 64) or not _token(selection["stable_id"], 256)
            or not _token(selection["class"], 256)
            or type(selection["matched_facts"]) is not dict or selection["matched_facts"]
            or selection["access"] != "execution_restricted"
            or selection["enforcement"] != "character_device_grant"):
        raise NvmlObservationError("not an empty-fact retained character-device selection")
    for key in ("observation_contract_digest", "device_binding_digest"):
        if not isinstance(selection[key], str) or re.fullmatch(r"[0-9a-f]{64}", selection[key]) is None:
            raise NvmlObservationError("noncanonical existing selection comparison digest")
    devices = selection["character_devices"]
    if type(devices) is not list or not 1 <= len(devices) <= 32:
        raise NvmlObservationError("missing or excessive retained character-device identities")
    previous = ""
    destinations, identities, primaries = set(), set(), []
    selected = None
    for device in devices:
        if (type(device) is not dict or set(device) != _DEVICE_KEYS
                or not _token(device["role"], 64) or device["role"] <= previous
                or device["access"] != "read_write"
                or type(device["major"]) is not int or not 1 <= device["major"] <= 0xffffffff
                or type(device["minor"]) is not int or not 0 <= device["minor"] <= 0xffffffff):
            raise NvmlObservationError("invalid, unsorted or repeated retained character identity")
        previous = device["role"]
        destination = device["destination"]
        if (not isinstance(destination, str) or not destination.startswith("/dev/")
                or len(destination) > 4096 or any(part in ("", ".", "..") for part in destination[1:].split("/"))
                or any(ord(char) < 32 or ord(char) == 127 for char in destination)):
            raise NvmlObservationError("invalid retained device destination")
        identity = (device["major"], device["minor"])
        if destination in destinations or identity in identities:
            raise NvmlObservationError("ambiguous retained destination or device alias")
        destinations.add(destination); identities.add(identity)
        if destination.startswith("/dev/nvidia-caps/"):
            raise NvmlObservationError("MIG capability assignment is outside this product")
        # Both a primary-looking path and any primary-range195 rdev count: a
        # renamed second GPU cannot hide behind an unrelated role/destination.
        if re.fullmatch(r"/dev/nvidia[0-9]+", destination) or (device["major"] == _PRIMARY_MAJOR and device["minor"] < 254):
            primaries.append(device)
        if device["role"] == role: selected = device
    if (selected is None or len(primaries) != 1 or selected is not primaries[0]
            or selected["major"] != _PRIMARY_MAJOR or selected["minor"] >= 254
            or selected["destination"] != f'/dev/nvidia{selected["minor"]}'):
        raise NvmlObservationError("missing or ambiguous selected NVIDIA primary character device")
    # Snapshot scalar comparison data before any fallible native call. Do not
    # retain caller-mutable DTOs or infer authority from any of these strings.
    return selection["stable_id"], selection["device_binding_digest"], dict(selected)


def observe_assignment(*, request: dict, bindings) -> dict:
    """Measure the primary identity of one raw ExecutionResourceSelection.

    Bindings must already be selected under D's independently admitted exact
    provider and original finite qualification owner. None or absent APIs refuse
    before initialization. Strict first subset requires positive non-MIG query
    results: NOT_SUPPORTED is not treated as disabled. NVML index is not CUDA
    ordinal; UUID equivalence and same-handle CUDA verification are separate gates.
    """
    stable_id, binding_digest, primary = _assignment(request)
    functions = {}
    try:
        for name, arguments in _PROTOTYPES.items():
            function = getattr(bindings, name)
            if not callable(function): raise TypeError(name)
            # A retained ctypes provider needs exact ABI translation; a pure
            # injected callable consumes the same ctypes arguments in tests.
            if isinstance(function, ctypes._CFuncPtr):
                function.argtypes = list(arguments)
                function.restype = ctypes.c_int
            functions[name] = function
    except (AttributeError, TypeError) as error:
        raise NvmlObservationError("selected NVML provider lacks finite required ABI") from error

    def checked(name, *args):
        try:
            result = functions[name](*args)
        except Exception as error:
            raise NvmlObservationError(f"unknown native outcome in {name}") from error
        if type(result) is not int or result != 0:
            raise NvmlObservationError(f"native observation refused in {name}")

    # Once initialization is attempted, attempt exactly one shutdown on every
    # path (including unknown init outcome). No retries or result on failed close.
    try:
        checked("nvmlInit_v2")
        count = ctypes.c_uint(_UINT_UNKNOWN)
        checked("nvmlDeviceGetCount_v2", ctypes.byref(count))
        if count.value != 1:
            raise NvmlObservationError("raw observer requires exactly one NVML device")
        handle = _HANDLE()
        checked("nvmlDeviceGetHandleByIndex_v2", 0, ctypes.byref(handle))
        if not handle.value: raise NvmlObservationError("missing selected physical handle")
        is_mig, current, pending = (ctypes.c_uint(_UINT_UNKNOWN) for _ in range(3))
        checked("nvmlDeviceIsMigDeviceHandle", handle, ctypes.byref(is_mig))
        if is_mig.value != 0: raise NvmlObservationError("MIG or unknown device handle")
        checked("nvmlDeviceGetMigMode", handle, ctypes.byref(current), ctypes.byref(pending))
        if current.value != 0 or pending.value != 0:
            raise NvmlObservationError("MIG enabled, pending or unobserved")
        minor = ctypes.c_uint(_UINT_UNKNOWN)
        checked("nvmlDeviceGetMinorNumber", handle, ctypes.byref(minor))
        if minor.value != primary["minor"]:
            raise NvmlObservationError("NVML device differs from retained primary minor")
        uuid = ctypes.create_string_buffer(b"\xff" * _UUID_BYTES, _UUID_BYTES)
        checked("nvmlDeviceGetUUID", handle, uuid, _UUID_BYTES)
        raw = uuid.raw
        # GPU-prefixed canonical physical UUID only; no MIG/name/PCI fallback.
        if raw.find(b"\0") != 40 or re.fullmatch(rb"GPU-[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}", raw[:40]) is None:
            raise NvmlObservationError("missing or noncanonical physical GPU UUID")
        uuid_hex = raw[4:40].replace(b"-", b"").decode("ascii")
        if uuid_hex == "0" * 32: raise NvmlObservationError("zero physical GPU UUID")
    finally:
        checked("nvmlShutdown")
    return {"version": 1, "stable_resource_id": stable_id,
            "device_binding_digest": binding_digest, "primary_device": primary,
            "uuid_v2_hex": uuid_hex, "nvml_shutdown": "closed"}
