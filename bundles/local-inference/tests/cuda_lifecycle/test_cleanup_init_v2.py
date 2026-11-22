"""Strict private init-v2 comparisons; no native provider or owner authority."""
from __future__ import annotations

import json
import sys
from pathlib import Path
import unittest

SOURCE = Path(__file__).resolve().parents[2] / '.ai/workers/local-inference/lib/local-tinygrad-gnu'
sys.path.insert(0, str(SOURCE))
import cleanup_channel as wire


BOUNDS = dict(maximum_entries=100, maximum_depth=10,
              maximum_file_bytes=1024, maximum_total_bytes=4096)
BINDINGS = {key: format(index, '064x') for index, key in enumerate(sorted(wire.BINDINGS), 1)}


def init():
    return dict(BINDINGS, version=2, kind='init',
                uuid_v2_hex='00112233445566778899aabbccddeeff',
                gnu_bounds=BOUNDS, driver_bounds=BOUNDS)


def encoded(value):
    return json.dumps(value, separators=(',', ':')).encode('utf-8')


class CleanupInitV2Tests(unittest.TestCase):
    def test_exact_init_returns_frozen_typed_comparisons(self):
        selected = wire.decode_init(encoded(init()))
        self.assertEqual(dict(selected.bindings), BINDINGS)
        self.assertEqual(selected.uuid_v2_hex, '00112233445566778899aabbccddeeff')
        self.assertEqual(selected.gnu_bounds.maximum_entries, 100)
        self.assertEqual(selected.driver_bounds.maximum_total_bytes, 4096)
        with self.assertRaises(TypeError): selected.bindings['process_owner'] = 'f' * 64

    def test_missing_foreign_or_noncanonical_init_refuses(self):
        valid = init()
        variants = [dict(valid, version=1), dict(valid, version=True),
                    dict(valid, kind='shutdown'), dict(valid, unknown=1),
                    dict(valid, uuid_v2_hex='0' * 32),
                    dict(valid, uuid_v2_hex='GPU-' + valid['uuid_v2_hex']),
                    dict(valid, uuid_v2_hex=valid['uuid_v2_hex'].upper()),
                    dict(valid, gnu_bounds=dict(BOUNDS, maximum_entries=True)),
                    dict(valid, driver_bounds=dict(BOUNDS, maximum_depth=0)),
                    dict(valid, driver_bounds=dict(BOUNDS, maximum_file_bytes=8192))]
        for field in ('teardown', 'capsule_hash', 'cleanup_contract',
                      'process_owner', 'resource_set', 'uuid_v2_hex',
                      'gnu_bounds', 'driver_bounds'):
            variants.append({key: value for key, value in valid.items() if key != field})
        for candidate in variants:
            with self.subTest(candidate=candidate), self.assertRaises(ValueError):
                wire.decode_init(encoded(candidate))

    def test_duplicate_nested_key_nonfinite_and_v1_shutdown_shape_refuse(self):
        body = encoded(init())
        duplicate = body.replace(b'"maximum_entries":100',
                                 b'"maximum_entries":100,"maximum_entries":100', 1)
        for raw in (duplicate, body[:-1] + b',"extra":0}',
                    body.replace(b'"maximum_depth":10', b'"maximum_depth":NaN', 1),
                    b'[]'):
            with self.subTest(raw=raw[:80]), self.assertRaises(ValueError):
                wire.decode_init(raw)
        with self.assertRaises(ValueError):
            wire.decode(encoded(init()), 'shutdown')


if __name__ == '__main__': unittest.main()
