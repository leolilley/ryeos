# ryeos-tool:
#   category: local-inference/gnu-cuda-driver-qualification
#   name: verify
#   version: "1.0.0"
#   description: Verify static GNU-first CUDA/NVML closure and selected GNU Python ABI
#   executor_id: tool:local-inference/gnu-cuda-driver-qualification/runtime
#   execution_protocol: protocol:ryeos/core/opaque
#   effects: live
#   filesystem_authority: captured_execution
#   network_authority: isolated
#   config_schema:
#     type: object
#     properties:
#       schema: {type: string, const: ryeos.gnu_cuda_driver_qualification_policy.v1}
#     required: [schema]
#     additionalProperties: false
#   external_content:
#     - id: producer-python
#       kind: tree
#       mode: pinned
#       digest: 800d4969489634cc3bbc5774bd9e99a330cdc23bbc1fd0fd231ec6a88ca9acdf
#       mount_root: execution_runtime
#       mount: producer-python
#   external_product_slots:
#     - id: subject
#       relationship_ref: config:ryeos-runtime/gnu-cuda-products-draft
#       relationship: driver_to_qualification_verifier
#       kind: tree
#       mount_root: execution_runtime
#       mount: driver
#     - id: gnu_runtime
#       relationship_ref: config:ryeos-runtime/gnu-cuda-products-draft
#       relationship: gnu_runtime_to_qualification_verifier
#       kind: tree
#       mount_root: execution_runtime
#       mount: python-gnu-private
"""Offline static qualification for one exact retained driver/GNU pair."""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import re
import selectors
import subprocess
import sys
import time

from driver_closure import DriverClosureError, TreeBounds, inspect_cuda_driver_closure


RESULT_SCHEMA = 'ryeos.product_qualification_result.v1'
CLAIM = 'gnu_user_driver_static_closure_v1'
POLICY_SCHEMA = 'ryeos.gnu_cuda_driver_qualification_policy.v1'
EVIDENCE_SCHEMA = 'ryeos.gnu_cuda_driver_static_closure_evidence.v1'
PRODUCER_PYTHON_HASH = '800d4969489634cc3bbc5774bd9e99a330cdc23bbc1fd0fd231ec6a88ca9acdf'
GNU_BOUNDS = {'maximum_entries': 10000, 'maximum_depth': 64, 'maximum_file_bytes': 134217728, 'maximum_total_bytes': 268435456}
DRIVER_BOUNDS = {'maximum_entries': 512, 'maximum_depth': 32, 'maximum_file_bytes': 268435456, 'maximum_total_bytes': 1073741824}
HASH = re.compile(r'[0-9a-f]{64}\Z')


class QualificationRefused(ValueError):
    pass


def _realizations() -> dict[str, dict[str, object]]:
    try:
        raw = json.loads(os.environ.get('RYEOS_EXTERNAL_REALIZATIONS', 'null'))
    except json.JSONDecodeError as error:
        raise QualificationRefused('sealed verifier realization selection is invalid') from error
    keys = {'id', 'kind', 'mode', 'manifest_hash', 'entry_count', 'total_bytes', 'mount_root', 'mount'}
    if (not isinstance(raw, list) or len(raw) != 3
            or any(not isinstance(item, dict) or set(item) != keys for item in raw)):
        raise QualificationRefused('verifier requires its exact Python, driver, and GNU selections')
    selected = {item['id']: item for item in raw}
    if set(selected) != {'producer-python', 'subject', 'gnu_runtime'}:
        raise QualificationRefused('verifier selected an unexpected product set')
    python, subject, gnu = selected['producer-python'], selected['subject'], selected['gnu_runtime']
    if (python['kind'] != 'tree' or python['mode'] != 'pinned'
            or python['mount_root'] != 'execution_runtime' or python['mount'] != 'producer-python'
            or python['manifest_hash'] != PRODUCER_PYTHON_HASH):
        raise QualificationRefused('verifier pinned Python realization changed')
    for item, mount, total_bound in (
        (subject, 'driver', 1073741824),
        (gnu, 'python-gnu-private', 268435456),
    ):
        if (item['kind'] != 'tree' or item['mode'] != 'pinned'
                or item['mount_root'] != 'execution_runtime' or item['mount'] != mount
                or not isinstance(item['manifest_hash'], str) or HASH.fullmatch(item['manifest_hash']) is None
                or type(item['entry_count']) is not int or not 1 <= item['entry_count'] <= 10000
                or type(item['total_bytes']) is not int or not 1 <= item['total_bytes'] <= total_bound):
            raise QualificationRefused(f'{mount} realization identity/bounds are unresolved')
    return selected


def _gnu_abi(root: Path) -> dict[str, object]:
    python = root / 'python' / 'bin' / 'python3.14'
    loader = root / 'python' / 'lib' / 'ld-linux-x86-64.so.2'
    if not python.is_file() or not loader.is_file():
        raise QualificationRefused('selected GNU Python interpreter/loader is absent')
    code = ('import json,sys,sysconfig; print(json.dumps({"version":list(sys.version_info[:3]),'
            '"implementation":sys.implementation.name,"cache_tag":sys.implementation.cache_tag,'
            '"soabi":sysconfig.get_config_var("SOABI"),"prefix":sys.prefix},separators=(",",":")))')
    deadline = time.monotonic() + 10
    process = None
    selector = None
    output = bytearray()
    try:
        process = subprocess.Popen(
            [str(loader), '--inhibit-cache', '--library-path', str(root / 'python' / 'lib'),
             str(python), '-P', '-S', '-B', '-c', code],
            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
            env={'PATH': '', 'PYTHONHOME': str(root / 'python'),
                 'PYTHONNOUSERSITE': '1', 'PYTHONSAFEPATH': '1',
                 'PYTHONDONTWRITEBYTECODE': '1', 'LC_ALL': 'C'},
        )
        selector = selectors.DefaultSelector()
        selector.register(process.stdout, selectors.EVENT_READ)
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not selector.select(remaining):
                raise QualificationRefused('selected GNU Python ABI probe exceeded its deadline')
            chunk = os.read(process.stdout.fileno(), min(4096, 4097 - len(output)))
            if not chunk:
                break
            output.extend(chunk)
            if len(output) > 4096:
                raise QualificationRefused('selected GNU Python ABI probe exceeded its output bound')
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise QualificationRefused('selected GNU Python ABI probe exceeded its deadline')
        returncode = process.wait(timeout=remaining)
        if time.monotonic() >= deadline:
            raise QualificationRefused('selected GNU Python ABI probe exceeded its deadline')
    except BaseException as error:
        if process is not None:
            try:
                process.kill()
            except OSError:
                pass
            try:
                process.wait(timeout=1)
            except subprocess.TimeoutExpired as settlement_error:
                raise QualificationRefused(
                    'ABI probe child cleanup exceeded its local deadline; outer process settlement must retain it'
                ) from settlement_error
        if isinstance(error, QualificationRefused):
            raise
        if isinstance(error, (OSError, subprocess.SubprocessError)):
            raise QualificationRefused('selected GNU Python ABI probe did not complete within bounds') from error
        raise
    finally:
        if selector is not None:
            selector.close()
        if process is not None and process.stdout is not None:
            process.stdout.close()
    if returncode != 0:
        raise QualificationRefused('selected GNU Python ABI probe failed')
    try:
        abi = json.loads(output)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise QualificationRefused('selected GNU Python ABI probe returned invalid JSON') from error
    expected_prefix = str((root / 'python').resolve(strict=True))
    if (not isinstance(abi, dict) or set(abi) != {'version', 'implementation', 'cache_tag', 'soabi', 'prefix'}
            or abi['version'] != [3, 14, 7] or abi['implementation'] != 'cpython'
            or abi['cache_tag'] != 'cpython-314' or not isinstance(abi['soabi'], str)
            or abi['prefix'] != expected_prefix):
        raise QualificationRefused('selected GNU Python ABI differs from the admitted product')
    return abi


def verify(request: object) -> dict[str, object]:
    if type(request) is not dict or set(request) != {'schema'} or request['schema'] != POLICY_SCHEMA:
        raise QualificationRefused('verifier parameters differ from the qualification policy schema')
    selected = _realizations()
    subject, gnu = selected['subject'], selected['gnu_runtime']
    driver_root = Path('/ryeos/realizations/driver')
    gnu_root = Path('/ryeos/realizations/python-gnu-private')
    try:
        closure = inspect_cuda_driver_closure(
            gnu_root / 'python' / 'lib', driver_root / 'lib',
            gnu_bounds=TreeBounds.from_contract(GNU_BOUNDS),
            driver_bounds=TreeBounds.from_contract(DRIVER_BOUNDS),
            entry_sonames=('libcuda.so.1', 'libnvidia-ml.so.1'),
        )
    except DriverClosureError as error:
        raise QualificationRefused(str(error)) from error
    abi = _gnu_abi(gnu_root)
    closure_binding = {
        'providers': {
            name: {'path': str(path), 'sha256': hashlib.sha256(path.read_bytes()).hexdigest()}
            for name, path in closure.providers.items()
        },
        'load_order': [str(path) for path in closure.load_order],
    }
    evidence = {
        'schema': EVIDENCE_SCHEMA,
        'subject_manifest_hash': subject['manifest_hash'],
        'gnu_manifest_hash': gnu['manifest_hash'],
        'subject_bounds': DRIVER_BOUNDS,
        'gnu_bounds': GNU_BOUNDS,
        'requested_entry_sonames': ['libcuda.so.1', 'libnvidia-ml.so.1'],
        'closure': closure_binding,
        'gnu_python_abi': abi,
        'limits': {'abi_probe_timeout_seconds': 10, 'abi_probe_combined_output_bytes': 4096,
                   'local_child_cleanup_wait_seconds': 1},
        'claims_scope': 'static_selected_tree_and_python_abi_only',
        'excluded_claims': ['symbol_version_compatibility', 'driver_api_initialization',
                            'device_identity', 'runtime_execution', 'performance'],
    }
    return {'schema': RESULT_SCHEMA, 'subject_manifest_hash': subject['manifest_hash'],
            'claims': [CLAIM], 'probe_evidence': evidence}


def main() -> int:
    try:
        request = json.load(sys.stdin)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise QualificationRefused('verifier request is not valid JSON') from error
    print(json.dumps(verify(request), sort_keys=True, separators=(',', ':')))
    return 0


if __name__ == '__main__':
    try:
        raise SystemExit(main())
    except QualificationRefused as error:
        print(f'qualification refused: {error}', file=sys.stderr)
        raise SystemExit(2)
