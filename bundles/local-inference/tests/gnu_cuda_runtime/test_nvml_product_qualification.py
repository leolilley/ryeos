import ctypes
import unittest

from nvml_product_qualification import (
    CLAIM,
    EVIDENCE_SCHEMA,
    POLICY_SCHEMA,
    QualificationInputError,
    qualify_raw_assignment,
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

    def test_returns_existing_product_qualification_shape_after_observer_success(self):
        result = qualify_raw_assignment(
            subject_manifest_hash="c" * 64,
            verifier_parameters=self.parameters,
            resource_selection=self.selection,
            bindings=self._bindings(),
        )
        self.assertEqual(result["claims"], [CLAIM])
        self.assertEqual(result["probe_evidence"]["schema"], EVIDENCE_SCHEMA)
        assignment = result["probe_evidence"]["assignment"]
        self.assertEqual(assignment["stable_resource_id"], "l4-primary")
        self.assertEqual(assignment["primary_device"], self.selection["character_devices"][0])
        self.assertEqual(assignment["uuid_v2_hex"], "0123456789abcdef0123456789abcdef")

    def test_unresolved_policy_refuses_before_native_bindings(self):
        unresolved = dict(self.parameters, primary_device_role=None)
        with self.assertRaises(QualificationInputError):
            qualify_raw_assignment(
                subject_manifest_hash="c" * 64,
                verifier_parameters=unresolved,
                resource_selection=self.selection,
                bindings=None,
            )


if __name__ == "__main__":
    unittest.main()
