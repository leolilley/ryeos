"""Finite product observer under fake ABI only; no provider/library/device use."""
from __future__ import annotations

import copy
import ctypes
import json
from pathlib import Path
import sys
from types import SimpleNamespace
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / '.ai/workers/local-inference/lib/local-tinygrad-gnu'))
from cuda_lifecycle.nvml_observer import (
    NvmlObservationError, observe_assignment, required_symbols, _PROTOTYPES,
)


def request():
    return {"resource_selection": {
        "stable_id": "selected-gpu", "class": "gpu", "matched_facts": {},
        "observation_contract_digest": "a" * 64, "device_binding_digest": "b" * 64,
        "access": "execution_restricted", "enforcement": "character_device_grant",
        "character_devices": [
            {"role": "control", "destination": "/dev/nvidiactl", "access": "read_write", "major": 195, "minor": 255},
            {"role": "primary", "destination": "/dev/nvidia7", "access": "read_write", "major": 195, "minor": 7},
            {"role": "uvm", "destination": "/dev/nvidia-uvm", "access": "read_write", "major": 511, "minor": 0},
        ]}, "primary_device_role": "primary"}


class FakeNvml:
    """Writes actual ctypes out-parameters, validates exact same opaque handle."""
    names = ("nvmlInit_v2", "nvmlDeviceGetCount_v2", "nvmlDeviceGetHandleByIndex_v2",
             "nvmlDeviceIsMigDeviceHandle", "nvmlDeviceGetMigMode",
             "nvmlDeviceGetMinorNumber", "nvmlDeviceGetUUID", "nvmlShutdown")

    def __init__(self):
        self.calls, self.handles = [], []
        self.count, self.handle, self.minor = 1, 0xabcdef012345, 7
        self.is_mig, self.current, self.pending = 0, 0, 0
        self.uuid = b"GPU-00112233-4455-6677-8899-aabbccddeeff\0"
        self.failures, self.unwritten = {}, set()
        self.on_call = None
        for name in self.names:
            setattr(self, name, self.function(name))

    @staticmethod
    def put(pointer, value):
        ctypes.cast(pointer, ctypes.POINTER(ctypes.c_uint))[0] = value

    def function(self, name):
        def call(*args):
            self.calls.append(name)
            if self.on_call: self.on_call(name)
            failure = self.failures.get(name)
            if isinstance(failure, BaseException): raise failure
            if failure is not None: return failure
            if name in self.unwritten: return 0
            if name == "nvmlDeviceGetCount_v2": self.put(args[0], self.count)
            elif name == "nvmlDeviceGetHandleByIndex_v2":
                if args[0] != 0: raise AssertionError("extra NVML index")
                ctypes.cast(args[1], ctypes.POINTER(ctypes.c_void_p))[0] = self.handle
            elif name not in {"nvmlInit_v2", "nvmlShutdown"}:
                handle = args[0].value if isinstance(args[0], ctypes.c_void_p) else args[0]
                if handle != self.handle: raise AssertionError("changed physical handle")
                self.handles.append((name, handle))
                if name == "nvmlDeviceIsMigDeviceHandle": self.put(args[1], self.is_mig)
                elif name == "nvmlDeviceGetMigMode":
                    self.put(args[1], self.current); self.put(args[2], self.pending)
                elif name == "nvmlDeviceGetMinorNumber": self.put(args[1], self.minor)
                else:
                    if args[2] != 96: raise AssertionError("unbounded UUID buffer")
                    ctypes.memmove(args[1], self.uuid, len(self.uuid))
            return 0
        return call


class NvmlObserverTests(unittest.TestCase):
    def setUp(self):
        self.no_load = mock.patch("ctypes.CDLL", side_effect=AssertionError("ambient library loading"))
        self.no_load.start(); self.addCleanup(self.no_load.stop)
        self.driver = FakeNvml()
        self.input = request()

    def observe(self):
        return observe_assignment(request=self.input, bindings=self.driver)

    def test_finite_required_symbol_api_matches_observer_prototype_table(self):
        self.assertEqual(required_symbols(), tuple(_PROTOTYPES))
        self.assertEqual(required_symbols(None), tuple(_PROTOTYPES))
        for invalid in (True, 'NVIDIA L4', 'l4-is-mig-success-zero-mode-not-supported-v1', {}, 1):
            with self.subTest(invalid=invalid), self.assertRaises(NvmlObservationError):
                required_symbols(invalid)
        self.assertFalse(self.driver.calls)

    def test_join_same_physical_handle_uuid_and_retained_primary_minor(self):
        result = self.observe()
        self.assertEqual(result, {
            "version": 1, "stable_resource_id": "selected-gpu", "device_binding_digest": "b" * 64,
            "primary_device": self.input["resource_selection"]["character_devices"][1],
            "uuid_v2_hex": "00112233445566778899aabbccddeeff", "nvml_shutdown": "closed"})
        self.assertEqual(self.driver.calls, list(FakeNvml.names))
        self.assertEqual({handle for _, handle in self.driver.handles}, {self.driver.handle})
        self.assertLess(len(json.dumps(result).encode()), 4096)
        self.assertNotIn("process_owner", result)  # no invented occurrence authority

    def test_enumeration_count_one_cannot_substitute_for_primary_identity(self):
        self.driver.minor = 0  # NVML index zero is NOT the retained minor seven
        with self.assertRaisesRegex(NvmlObservationError, "retained primary minor"): self.observe()
        self.assertEqual(self.driver.calls[-1], "nvmlShutdown")
        self.assertNotIn("nvmlDeviceGetUUID", self.driver.calls)

    def test_ctypes_provider_translation_preserves_wide_handle_and_out_parameters(self):
        # Python callbacks behind C ABI trampolines exercise the actual ctypes
        # binding branch, without loading a DLL or using a native NVML provider.
        provider = SimpleNamespace()
        for name, arguments in _PROTOTYPES.items():
            callback = ctypes.CFUNCTYPE(ctypes.c_int, *arguments)(self.driver.function(name))
            setattr(provider, name, callback)
        result = observe_assignment(request=self.input, bindings=provider)
        self.assertEqual(result["uuid_v2_hex"], "00112233445566778899aabbccddeeff")
        self.assertEqual({handle for _, handle in self.driver.handles}, {0xabcdef012345})
        self.assertEqual(self.driver.calls, list(FakeNvml.names))
        for name, arguments in _PROTOTYPES.items():
            self.assertEqual(getattr(provider, name).argtypes, list(arguments))
            self.assertIs(getattr(provider, name).restype, ctypes.c_int)

    def test_missing_provider_or_any_required_query_refuses_before_initialization(self):
        with self.assertRaises(NvmlObservationError): observe_assignment(request=self.input, bindings=None)
        for name in FakeNvml.names:
            with self.subTest(name=name):
                driver = FakeNvml(); delattr(driver, name)
                with self.assertRaises(NvmlObservationError): observe_assignment(request=self.input, bindings=driver)
                self.assertFalse(driver.calls)

    def test_missing_ambiguous_or_foreign_raw_assignment_refuses_before_native_use(self):
        selection = self.input["resource_selection"]
        invalid = [None, {}, {**self.input, "expected_uuid": "copyable"}]
        def changed(key, value):
            candidate = request(); candidate["resource_selection"][key] = value; invalid.append(candidate)
        for key, value in (("matched_facts", {"uuid": "authored"}), ("access", "deployment_visible"),
                           ("enforcement", "deployment_visible"), ("character_devices", []),
                           ("device_binding_digest", "B"*64), ("stable_id", "bad/id")):
            changed(key, value)
        candidate = request(); candidate["primary_device_role"] = "control"; invalid.append(candidate)
        for field, value in (("major", 196), ("major", True), ("minor", -1), ("minor", 255),
                             ("access", "read_only"), ("destination", "/dev/nvidia0"),
                             ("destination", "/dev/../nvidia7"), ("destination", "/dev/nvidia07")):
            candidate = request(); candidate["resource_selection"]["character_devices"][1][field] = value
            invalid.append(candidate)
        changed("character_devices", list(reversed(selection["character_devices"])))
        changed("character_devices", [*selection["character_devices"], selection["character_devices"][1]])
        for candidate in invalid:
            with self.subTest(candidate=candidate), self.assertRaises(NvmlObservationError):
                observe_assignment(request=candidate, bindings=self.driver)
        self.assertFalse(self.driver.calls)

    def test_second_primary_alias_or_mig_capability_assignment_refuses(self):
        for destination, major, minor in (("/dev/nvidia8", 195, 8), ("/dev/renamed-gpu", 195, 8),
                                         ("/dev/alias", 195, 7), ("/dev/nvidia-caps/mig1", 510, 1)):
            candidate = request()
            candidate["resource_selection"]["character_devices"].insert(2,
                {"role":"secondary", "destination":destination, "access":"read_write", "major":major, "minor":minor})
            with self.subTest(destination=destination), self.assertRaises(NvmlObservationError):
                observe_assignment(request=candidate, bindings=self.driver)
        self.assertFalse(self.driver.calls)

    def test_multiple_no_or_unknown_nvml_devices_refuse_without_selection(self):
        for count in (0, 2, 0xffffffff):
            driver = FakeNvml(); driver.count = count
            with self.subTest(count=count), self.assertRaises(NvmlObservationError):
                observe_assignment(request=self.input, bindings=driver)
            self.assertEqual(driver.calls, ["nvmlInit_v2", "nvmlDeviceGetCount_v2", "nvmlShutdown"])

    def test_mig_handle_enabled_pending_or_unknown_modes_refuse(self):
        for attribute, value in (("is_mig", 1), ("is_mig", 0xffffffff), ("current", 1),
                                 ("pending", 1), ("current", 0xffffffff)):
            driver = FakeNvml(); setattr(driver, attribute, value)
            with self.subTest(attribute=attribute), self.assertRaises(NvmlObservationError):
                observe_assignment(request=self.input, bindings=driver)
            self.assertEqual(driver.calls[-1], "nvmlShutdown")
            self.assertNotIn("nvmlDeviceGetUUID", driver.calls)

    def test_noncanonical_missing_mig_or_zero_uuid_never_returns_candidate(self):
        for uuid in (b"MIG-00112233-4455-6677-8899-aabbccddeeff\0",
                     b"GPU-00112233-4455-6677-8899-AABBCCDDEEFF\0",
                     b"GPU-00000000-0000-0000-0000-000000000000\0", b"GPU-\0",
                     b"x"*96, b"GPU-00112233-4455-6677-8899-aabbccddeeff"):
            driver = FakeNvml(); driver.uuid = uuid
            with self.subTest(uuid=uuid), self.assertRaises(NvmlObservationError):
                observe_assignment(request=self.input, bindings=driver)
            self.assertEqual(driver.calls[-1], "nvmlShutdown")

    def test_native_errors_exceptions_or_unsupported_query_attempt_one_close_and_refuse(self):
        for name in FakeNvml.names:
            for failure in (3, True, RuntimeError("unknown native outcome")):
                driver = FakeNvml(); driver.failures[name] = failure
                with self.subTest(name=name, failure=failure), self.assertRaises(NvmlObservationError):
                    observe_assignment(request=self.input, bindings=driver)
                self.assertEqual(driver.calls.count("nvmlShutdown"), 1)
                self.assertEqual(driver.calls[-1], "nvmlShutdown")

    def test_success_status_without_output_never_passes(self):
        for name in FakeNvml.names[1:-1]:
            driver = FakeNvml(); driver.unwritten.add(name)
            with self.subTest(name=name), self.assertRaises(NvmlObservationError):
                observe_assignment(request=self.input, bindings=driver)
            self.assertEqual(driver.calls[-1], "nvmlShutdown")

    def test_shutdown_failure_after_complete_observation_cannot_return_success(self):
        self.driver.failures["nvmlShutdown"] = 999
        with self.assertRaisesRegex(NvmlObservationError, "nvmlShutdown"): self.observe()
        self.assertEqual(self.driver.calls, list(FakeNvml.names))

    def test_candidate_snapshots_input_and_does_not_mutate_retained_projection(self):
        original = copy.deepcopy(self.input)
        result = self.observe()
        self.assertEqual(self.input, original)
        result["primary_device"]["minor"] = 55
        self.assertEqual(self.input, original)
        self.driver.calls.clear()
        self.driver.on_call = lambda name: self.input["resource_selection"]["character_devices"][1].update(minor=55)
        result = self.observe()
        self.assertEqual(result["primary_device"]["minor"], 7)


if __name__ == "__main__": unittest.main()
