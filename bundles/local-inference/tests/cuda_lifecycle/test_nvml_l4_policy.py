"""Unselected L4 query profiles under injected ABI; no native qualification."""
from __future__ import annotations

import ctypes
from pathlib import Path
import sys
from types import SimpleNamespace
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / '.ai/workers/local-inference/lib/local-tinygrad-gnu'))
from cuda_lifecycle.nvml_observer import (
    L4QueryPolicy, NvmlObservationError, observe_assignment, required_symbols, _PROTOTYPES,
)
from test_nvml_observer import FakeNvml, request


class FakeL4(FakeNvml):
    def __init__(self):
        super().__init__()
        self.name = b"NVIDIA L4\0"
        self.failures["nvmlDeviceGetMigMode"] = 3
        self.query_statuses = []
        for name in (*self.names, "nvmlDeviceGetName"):
            underlying = self.get_name if name == "nvmlDeviceGetName" else getattr(self, name)
            setattr(self, name, self.record(name, underlying))

    def record(self, name, function):
        def call(*args):
            result = function(*args)
            self.query_statuses.append((name, result))
            return result
        return call

    def get_name(self, handle, output, size):
        self.calls.append("nvmlDeviceGetName")
        value = handle.value if isinstance(handle, ctypes.c_void_p) else handle
        if value != self.handle or size != 96: raise AssertionError("SKU query changed handle/bound")
        self.handles.append(("nvmlDeviceGetName", value))
        failure = self.failures.get("nvmlDeviceGetName")
        if isinstance(failure, BaseException): raise failure
        if failure is not None: return failure
        if "nvmlDeviceGetName" not in self.unwritten:
            ctypes.memmove(output, self.name, len(self.name))
        return 0


class L4PolicyTests(unittest.TestCase):
    def setUp(self):
        self.no_load = mock.patch("ctypes.CDLL", side_effect=AssertionError("native library prohibited"))
        self.no_load.start(); self.addCleanup(self.no_load.stop)
        self.driver, self.input = FakeL4(), request()

    def observe(self, policy=L4QueryPolicy.IS_MIG_SUCCESS_ZERO):
        return observe_assignment(request=self.input, bindings=self.driver, l4_query_policy=policy)

    def test_required_symbols_are_exact_for_only_the_selected_policy(self):
        base = tuple(_PROTOTYPES)
        self.assertEqual(required_symbols(None), base)
        for policy in (L4QueryPolicy.IS_MIG_SUCCESS_ZERO, L4QueryPolicy.IS_MIG_NOT_SUPPORTED):
            self.assertEqual(required_symbols(policy), base + ('nvmlDeviceGetName',))
        for invalid in (True, 'NVIDIA L4', L4QueryPolicy.IS_MIG_SUCCESS_ZERO.value, {}):
            with self.subTest(invalid=invalid), self.assertRaises(NvmlObservationError):
                required_symbols(invalid)
        self.assertFalse(self.driver.calls)

    def test_exact_success_zero_and_mode_unsupported_profile_keeps_same_handle_join(self):
        result = self.observe()
        self.assertEqual(result["uuid_v2_hex"], "00112233445566778899aabbccddeeff")
        self.assertEqual(result["primary_device"]["minor"], 7)
        self.assertEqual({value for _, value in self.driver.handles}, {self.driver.handle})
        self.assertEqual(self.driver.calls, ["nvmlInit_v2", "nvmlDeviceGetCount_v2",
            "nvmlDeviceGetHandleByIndex_v2", "nvmlDeviceGetName", "nvmlDeviceIsMigDeviceHandle",
            "nvmlDeviceGetMigMode", "nvmlDeviceGetMinorNumber", "nvmlDeviceGetUUID", "nvmlShutdown"])
        # Native qualification can retain actual injected-provider query statuses
        # OUTSIDE the unchanged candidate DTO; this fixture grants no claims.
        self.assertIn(("nvmlDeviceIsMigDeviceHandle", 0), self.driver.query_statuses)
        self.assertIn(("nvmlDeviceGetMigMode", 3), self.driver.query_statuses)
        self.assertEqual(set(result), {"version", "stable_resource_id", "device_binding_digest",
            "primary_device", "uuid_v2_hex", "nvml_shutdown"})

    def test_exact_both_unsupported_profile_requires_explicit_other_variant(self):
        self.driver.failures["nvmlDeviceIsMigDeviceHandle"] = 3
        result = self.observe(L4QueryPolicy.IS_MIG_NOT_SUPPORTED)
        self.assertEqual(result["nvml_shutdown"], "closed")
        self.assertIn(("nvmlDeviceIsMigDeviceHandle", 3), self.driver.query_statuses)
        self.assertIn(("nvmlDeviceGetMigMode", 3), self.driver.query_statuses)
        with self.assertRaises(NvmlObservationError): self.observe(L4QueryPolicy.IS_MIG_SUCCESS_ZERO)

    def test_no_profile_default_still_refuses_unsupported_without_sku_policy_discovery(self):
        for is_mig_status in (0, 3):
            self.driver.calls.clear()
            if is_mig_status == 3: self.driver.failures["nvmlDeviceIsMigDeviceHandle"] = 3
            with self.subTest(status=is_mig_status), self.assertRaises(NvmlObservationError): self.observe(None)
            self.assertNotIn("nvmlDeviceGetName", self.driver.calls)
            self.assertEqual(self.driver.calls[-1], "nvmlShutdown")

    def test_strict_default_success_keeps_original_finite_abi_and_candidate(self):
        driver = FakeNvml()
        result = observe_assignment(request=self.input, bindings=driver)
        self.assertEqual(result["nvml_shutdown"], "closed")
        self.assertEqual(driver.calls, list(FakeNvml.names))

    def test_l4_extra_ctypes_name_abi_preserves_wide_physical_handle(self):
        prototypes = {**_PROTOTYPES,
            "nvmlDeviceGetName": (ctypes.c_void_p, ctypes.POINTER(ctypes.c_char), ctypes.c_uint)}
        provider = SimpleNamespace()
        for name, arguments in prototypes.items():
            setattr(provider, name, ctypes.CFUNCTYPE(ctypes.c_int, *arguments)(getattr(self.driver, name)))
        result = observe_assignment(request=self.input, bindings=provider,
                                    l4_query_policy=L4QueryPolicy.IS_MIG_SUCCESS_ZERO)
        self.assertEqual(result["uuid_v2_hex"], "00112233445566778899aabbccddeeff")
        self.assertEqual({handle for _, handle in self.driver.handles}, {0xabcdef012345})
        self.assertEqual(provider.nvmlDeviceGetName.argtypes, list(prototypes["nvmlDeviceGetName"]))

    def test_raw_request_flag_name_status_or_copyable_policy_string_never_selects_profile(self):
        for policy in (True, "NVIDIA L4", L4QueryPolicy.IS_MIG_SUCCESS_ZERO.value, {}, 3):
            with self.subTest(policy=policy), self.assertRaises(NvmlObservationError): self.observe(policy)
        self.assertFalse(self.driver.calls)
        for key in ("l4_query_policy", "device_name", "expected_status"):
            candidate = {**self.input, key: "selected"}
            with self.subTest(key=key), self.assertRaises(NvmlObservationError):
                observe_assignment(request=candidate, bindings=self.driver, l4_query_policy=L4QueryPolicy.IS_MIG_SUCCESS_ZERO)
        self.assertFalse(self.driver.calls)

    def test_wrong_sku_mig_name_or_noncanonical_name_refuses_before_mig_queries(self):
        for name in (b"NVIDIA A100\0", b"NVIDIA L40\0", b"NVIDIA L4 MIG\0", b"NVIDIA L4", b"NVIDIA\0L4", b"\xff"*96):
            self.driver.calls.clear(); self.driver.name = name
            with self.subTest(name=name), self.assertRaises(NvmlObservationError): self.observe()
            self.assertNotIn("nvmlDeviceIsMigDeviceHandle", self.driver.calls)
            self.assertEqual(self.driver.calls[-1], "nvmlShutdown")

    def test_missing_or_unwritten_sku_query_cannot_infer_l4_from_unsupported(self):
        del self.driver.nvmlDeviceGetName
        with self.assertRaises(NvmlObservationError): self.observe()
        self.assertFalse(self.driver.calls)
        self.driver = FakeL4(); self.driver.unwritten.add("nvmlDeviceGetName")
        with self.assertRaises(NvmlObservationError): self.observe()
        self.assertNotIn("nvmlDeviceIsMigDeviceHandle", self.driver.calls)

    def test_other_error_is_not_global_unsupported_fallback(self):
        for name in ("nvmlDeviceGetName", "nvmlDeviceIsMigDeviceHandle", "nvmlDeviceGetMigMode",
                     "nvmlDeviceGetMinorNumber", "nvmlDeviceGetUUID"):
            for error in (2, 4, 999, True, RuntimeError("unknown")):
                self.driver = FakeL4(); self.driver.failures[name] = error
                with self.subTest(name=name, error=error), self.assertRaises(NvmlObservationError): self.observe()
                self.assertEqual(self.driver.calls.count("nvmlShutdown"), 1)

    def test_success_is_mig_must_write_zero_not_mig_or_unknown_output(self):
        self.driver.unwritten.add("nvmlDeviceIsMigDeviceHandle")
        with self.assertRaises(NvmlObservationError): self.observe()
        self.driver.unwritten.clear(); self.driver.is_mig = 1
        with self.assertRaises(NvmlObservationError): self.observe()
        self.assertNotIn("nvmlDeviceGetUUID", self.driver.calls)

    def test_each_profile_refuses_other_profile_status_even_when_same_name_matches(self):
        with self.assertRaises(NvmlObservationError): self.observe(L4QueryPolicy.IS_MIG_NOT_SUPPORTED)
        self.driver.failures["nvmlDeviceIsMigDeviceHandle"] = 3
        with self.assertRaises(NvmlObservationError): self.observe(L4QueryPolicy.IS_MIG_SUCCESS_ZERO)

    def test_l4_profile_expects_exact_mode_status_not_success_disabled_union(self):
        del self.driver.failures["nvmlDeviceGetMigMode"]  # success + current/pending zero
        with self.assertRaises(NvmlObservationError): self.observe()
        self.assertNotIn("nvmlDeviceGetMinorNumber", self.driver.calls)

    def test_physical_uuid_and_minor_checks_still_refuse_mig_or_foreign_identity(self):
        self.driver.uuid = b"MIG-00112233-4455-6677-8899-aabbccddeeff\0"
        with self.assertRaises(NvmlObservationError): self.observe()
        self.driver = FakeL4(); self.driver.minor = 0
        with self.assertRaises(NvmlObservationError): self.observe()
        self.assertNotIn("nvmlDeviceGetUUID", self.driver.calls)

    def test_unsupported_does_not_apply_to_uuid_minor_or_name(self):
        for name in ("nvmlDeviceGetName", "nvmlDeviceGetMinorNumber", "nvmlDeviceGetUUID"):
            self.driver = FakeL4(); self.driver.failures[name] = 3
            with self.subTest(name=name), self.assertRaises(NvmlObservationError): self.observe()
            self.assertEqual(self.driver.calls[-1], "nvmlShutdown")

    def test_close_failure_after_complete_l4_observation_refuses(self):
        self.driver.failures["nvmlShutdown"] = 3
        with self.assertRaises(NvmlObservationError): self.observe()
        self.assertIn("nvmlDeviceGetUUID", self.driver.calls)
        self.assertEqual(self.driver.calls[-1], "nvmlShutdown")


if __name__ == "__main__": unittest.main()
