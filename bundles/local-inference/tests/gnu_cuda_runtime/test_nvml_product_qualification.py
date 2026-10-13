import ctypes
from unittest.mock import patch
import unittest

from nvml_product_qualification import (
    POLICY_SCHEMA,
    QualificationInputError,
    observe_preflight_candidate,
    required_preflight_symbols,
    resolve_query_policy,
)


class NvmlProductQualificationTests(unittest.TestCase):
    def setUp(self):
        self.selection = {
            "stable_id": "l4-primary",
            "class": "gpu-l4",
            "matched_facts": {},
            "observation_contract_digest": "a" * 64,
            "device_binding_digest": "b" * 64,
            "access": "execution_restricted",
            "enforcement": "character_device_grant",
            "character_devices": [{
                "role": "gpu-primary",
                "destination": "/dev/nvidia0",
                "access": "read_write",
                "major": 195,
                "minor": 0,
            }],
        }
        self.parameters = {
            "schema": POLICY_SCHEMA,
            "primary_device_role": "gpu-primary",
            "l4_query_policy": None,
        }
        self.observation = {
            "version": 1,
            "stable_resource_id": "l4-primary",
            "device_binding_digest": "b" * 64,
            "primary_device": self.selection["character_devices"][0],
            "uuid_v2_hex": "0123456789abcdef0123456789abcdef",
            "nvml_shutdown": "closed",
        }

    @staticmethod
    def _set_uint(pointer, value):
        ctypes.cast(pointer, ctypes.POINTER(ctypes.c_uint))[0] = value

    def _bindings(self):
        class Bindings:
            def nvmlInit_v2(inner):
                return 0

            def nvmlDeviceGetCount_v2(inner, count):
                self._set_uint(count, 1)
                return 0

            def nvmlDeviceGetHandleByIndex_v2(inner, index, handle):
                ctypes.cast(handle, ctypes.POINTER(ctypes.c_void_p))[0] = ctypes.c_void_p(0x1234)
                return 0 if index == 0 else 1

            def nvmlDeviceIsMigDeviceHandle(inner, handle, is_mig):
                self._set_uint(is_mig, 0)
                return 0

            def nvmlDeviceGetMigMode(inner, handle, current, pending):
                self._set_uint(current, 0)
                self._set_uint(pending, 0)
                return 0

            def nvmlDeviceGetMinorNumber(inner, handle, minor):
                self._set_uint(minor, 0)
                return 0

            def nvmlDeviceGetUUID(inner, handle, output, length):
                raw = b"GPU-01234567-89ab-cdef-0123-456789abcdef\0"
                ctypes.memmove(output, raw, len(raw))
                return 0

            def nvmlShutdown(inner):
                return 0

        return Bindings()

    def test_returns_only_existing_observer_dto_after_injected_driver_shutdown(self):
        bindings = self._bindings()
        calls = []
        for name in ("nvmlInit_v2", "nvmlDeviceGetCount_v2", "nvmlDeviceGetHandleByIndex_v2",
                     "nvmlDeviceIsMigDeviceHandle", "nvmlDeviceGetMigMode",
                     "nvmlDeviceGetMinorNumber", "nvmlDeviceGetUUID", "nvmlShutdown"):
            original = getattr(bindings, name)
            def traced(*args, _original=original, _name=name):
                calls.append(_name)
                return _original(*args)
            setattr(bindings, name, traced)
        result = observe_preflight_candidate(self.parameters, self.selection, bindings)
        self.assertEqual(set(result), {
            "version", "stable_resource_id", "device_binding_digest",
            "primary_device", "uuid_v2_hex", "nvml_shutdown",
        })
        self.assertEqual(result["version"], 1)
        self.assertEqual(result["stable_resource_id"], "l4-primary")
        self.assertEqual(result["primary_device"], self.selection["character_devices"][0])
        self.assertEqual(result["uuid_v2_hex"], "0123456789abcdef0123456789abcdef")
        self.assertEqual(calls[-1], "nvmlShutdown")
        self.assertEqual(calls.count("nvmlShutdown"), 1)

    def test_bool_version_is_not_accepted_as_v1(self):
        from cuda_lifecycle import nvml_observer
        with patch.object(nvml_observer, "observe_assignment", return_value=dict(self.observation, version=True)):
            with self.assertRaises(QualificationInputError):
                observe_preflight_candidate(self.parameters, self.selection, object())

    def test_unresolved_policy_refuses_before_native_bindings(self):
        unresolved = dict(self.parameters, primary_device_role=None)
        with self.assertRaises(QualificationInputError):
            observe_preflight_candidate(unresolved, self.selection, None)

    def test_resolver_and_symbol_list_are_single_finite_source(self):
        from cuda_lifecycle import nvml_observer

        role, policy = resolve_query_policy(self.parameters)
        self.assertEqual(role, "gpu-primary")
        self.assertIsNone(policy)
        base = nvml_observer.required_symbols()
        self.assertEqual(required_preflight_symbols(self.parameters), base)
        self.assertIs(type(base), tuple)
        self.assertIn("nvmlInit_v2", base)
        self.assertIn("nvmlShutdown", base)
        self.assertNotIn("nvmlDeviceGetName", base)

        l4 = dict(self.parameters, l4_query_policy="l4-is-mig-success-zero-mode-not-supported-v1")
        _, selected = resolve_query_policy(l4)
        symbols = required_preflight_symbols(l4)
        self.assertIs(selected, nvml_observer.L4QueryPolicy.IS_MIG_SUCCESS_ZERO)
        self.assertEqual(symbols, base + ("nvmlDeviceGetName",))

    def test_invalid_or_ambiguous_policy_has_no_symbol_profile(self):
        from cuda_lifecycle import nvml_observer

        invalid = dict(self.parameters, l4_query_policy="not-a-qualified-profile")
        with self.assertRaises(QualificationInputError):
            required_preflight_symbols(invalid)
        with self.assertRaises(nvml_observer.NvmlObservationError):
            nvml_observer.required_symbols("not-an-enum")


if __name__ == "__main__":
    unittest.main()
