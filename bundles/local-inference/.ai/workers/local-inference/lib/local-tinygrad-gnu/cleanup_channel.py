"""Product side of D's private cleanup wire; frames confer no authority.

D/Lillux owns descriptor creation, inheritance, original deadline, process and
financial settlement. This executable interprets bounded messages on that
inherited stream using the same read/write mechanics as the existing session.
"""
from __future__ import annotations
from dataclasses import dataclass
import json
import os
import re
import stat
import struct
from types import MappingProxyType
from typing import Mapping

from driver_closure import TreeBounds

MAX_BYTES = 4096
BINDINGS = frozenset({'teardown', 'capsule_hash', 'cleanup_contract', 'process_owner', 'resource_set'})
KEYS = BINDINGS | {'version', 'kind'}
INIT_KEYS = KEYS | {'uuid_v2_hex', 'gnu_bounds', 'driver_bounds'}
PREFLIGHT_KEYS = frozenset({'version', 'kind', 'bindings', 'challenge', 'inputs'})
RESULT_KEYS = frozenset({'version', 'kind', 'bindings', 'challenge', 'observation'})
INPUT_KEYS = frozenset({'resource_selection', 'policy', 'handler_source_sha256',
                        'provider_manifest_sha256', 'gnu_bounds', 'driver_bounds'})
SELECTION_KEYS = frozenset({'stable_id', 'class', 'matched_facts',
    'observation_contract_digest', 'device_binding_digest', 'access',
    'enforcement', 'character_devices'})
DEVICE_KEYS = frozenset({'role', 'destination', 'access', 'major', 'minor'})
POLICY_KEYS = frozenset({'schema', 'primary_device_role', 'l4_query_policy'})
OBSERVATION_KEYS = frozenset({'version', 'stable_resource_id',
    'device_binding_digest', 'primary_device', 'uuid_v2_hex', 'nvml_shutdown'})
L4_POLICIES = frozenset({'l4-is-mig-success-zero-mode-not-supported-v1',
                       'l4-is-mig-not-supported-mode-not-supported-v1'})


@dataclass(frozen=True)
class CleanupInit:
    """Typed startup comparisons, never original owner or device authority."""
    bindings: Mapping[str, str]
    uuid_v2_hex: str
    gnu_bounds: TreeBounds
    driver_bounds: TreeBounds


@dataclass(frozen=True)
class PreflightRequest:
    """Bounded validated snapshot, not admission or a mutable owner claim."""
    _body: bytes

    @property
    def values(self):
        # Fresh detached comparison values; caller mutation cannot change custody.
        return strict_json(self._body)


def _hex(value, length):
    return type(value) is str and re.fullmatch('[0-9a-f]{%d}' % length, value) is not None


def _token(value, length):
    return (type(value) is str and 1 <= len(value) <= length
            and re.fullmatch(r'[A-Za-z0-9_.-]+', value) is not None)


def _bindings(value):
    if type(value) is not dict or set(value) != BINDINGS or any(not _hex(v, 64) for v in value.values()):
        raise ValueError('preflight original bindings malformed')


def _device(device):
    if (type(device) is not dict or set(device) != DEVICE_KEYS
            or not _token(device['role'], 64) or device['access'] != 'read_write'
            or type(device['major']) is not int or not 1 <= device['major'] <= 0xffffffff
            or type(device['minor']) is not int or not 0 <= device['minor'] <= 0xffffffff):
        raise ValueError('preflight device identity malformed')
    destination = device['destination']
    if (type(destination) is not str or len(destination.encode('utf-8')) > 4096
            or not destination.startswith('/dev/')
            or any(p in ('', '.', '..') for p in destination[1:].split('/'))
            or any(ord(c) < 32 or 127 <= ord(c) <= 159 for c in destination)
            or destination.startswith('/dev/nvidia-caps/')):
        raise ValueError('preflight device destination malformed')


def _inputs(inputs):
    if type(inputs) is not dict or set(inputs) != INPUT_KEYS:
        raise ValueError('preflight inputs not closed')
    for name in ('handler_source_sha256', 'provider_manifest_sha256'):
        if not _hex(inputs[name], 64): raise ValueError('preflight product comparison malformed')
    for name in ('gnu_bounds', 'driver_bounds'):
        bounds = inputs[name]
        TreeBounds.from_contract(bounds)
        if (bounds['maximum_entries'] > 0xffffffff or bounds['maximum_depth'] > 0xffffffff
                or bounds['maximum_file_bytes'] > 0xffffffffffffffff
                or bounds['maximum_total_bytes'] > 0xffffffffffffffff):
            raise ValueError('preflight bounds exceed wire integers')
    policy = inputs['policy']
    if (type(policy) is not dict or set(policy) != POLICY_KEYS
            or policy['schema'] != 'ryeos.local_tinygrad_nvml_assignment_policy.v1'
            or not _token(policy['primary_device_role'], 64)
            or (policy['l4_query_policy'] is not None and
                (type(policy['l4_query_policy']) is not str or policy['l4_query_policy'] not in L4_POLICIES))):
        raise ValueError('preflight finite policy malformed')
    selection = inputs['resource_selection']
    if (type(selection) is not dict or set(selection) != SELECTION_KEYS
            or not _token(selection['stable_id'], 256) or not _token(selection['class'], 256)
            or type(selection['matched_facts']) is not dict or selection['matched_facts']
            or selection['access'] != 'execution_restricted'
            or selection['enforcement'] != 'character_device_grant'
            or not _hex(selection['observation_contract_digest'], 64)
            or not _hex(selection['device_binding_digest'], 64)):
        raise ValueError('preflight raw selection malformed')
    devices = selection['character_devices']
    if type(devices) is not list or not 1 <= len(devices) <= 32:
        raise ValueError('preflight device count malformed')
    previous, destinations, identities, primaries = '', set(), set(), []
    for device in devices:
        _device(device)
        identity = (device['major'], device['minor'])
        if device['role'] <= previous or device['destination'] in destinations or identity in identities:
            raise ValueError('preflight device alias/order malformed')
        previous = device['role']; destinations.add(device['destination']); identities.add(identity)
        if (re.fullmatch(r'/dev/nvidia[0-9]+', device['destination']) is not None
                or (device['major'] == 195 and device['minor'] < 254)):
            primaries.append(device)
    if (len(primaries) != 1 or primaries[0]['role'] != policy['primary_device_role']
            or primaries[0]['major'] != 195 or primaries[0]['minor'] >= 254
            or primaries[0]['destination'] != '/dev/nvidia%d' % primaries[0]['minor']):
        raise ValueError('preflight primary selection mismatch')
    return primaries[0]


def decode_preflight(data: bytes) -> PreflightRequest:
    value = strict_json(data)
    if (type(value) is not dict or set(value) != PREFLIGHT_KEYS
            or type(value['version']) is not int or value['version'] != 3
            or value['kind'] != 'preflight'):
        raise ValueError('preflight request shape/version/kind mismatch')
    _bindings(value['bindings']); _inputs(value['inputs'])
    if not _hex(value['challenge'], 64) or value['challenge'] == '0' * 64:
        raise ValueError('preflight challenge malformed')
    return PreflightRequest(bytes(data))


def decode_preflight_result(data: bytes, request: PreflightRequest) -> dict:
    expected = decode_preflight(request._body).values
    value = strict_json(data)
    if (type(value) is not dict or set(value) != RESULT_KEYS
            or type(value['version']) is not int or value['version'] != 3
            or value['kind'] != 'preflight-result'
            or value['bindings'] != expected['bindings'] or value['challenge'] != expected['challenge']):
        raise ValueError('preflight result phase/challenge/owner mismatch')
    _bindings(value['bindings'])
    observation = value['observation']; selection = expected['inputs']['resource_selection']
    if (type(observation) is not dict or set(observation) != OBSERVATION_KEYS
            or type(observation['version']) is not int or observation['version'] != 1
            or observation['stable_resource_id'] != selection['stable_id']
            or observation['device_binding_digest'] != selection['device_binding_digest']
            or observation['primary_device'] != _inputs(expected['inputs'])
            or not _hex(observation['uuid_v2_hex'], 32) or observation['uuid_v2_hex'] == '0' * 32
            or observation['nvml_shutdown'] != 'closed'):
        raise ValueError('preflight observation mismatch')
    _device(observation['primary_device'])
    return observation  # candidate comparisons only, never a startup authority


def _read_body(fd):
    size = struct.unpack('>I', read_exact(fd, 4))[0]
    if not 0 < size <= MAX_BYTES: raise ValueError('private frame length exceeds bound')
    return read_exact(fd, size)


def _write_body(fd, data):
    if not 0 < len(data) <= MAX_BYTES: raise ValueError('private frame length exceeds bound')
    frame = struct.pack('>I', len(data)) + data
    offset = 0
    while offset < len(frame):
        try: count = os.write(fd, frame[offset:])
        except InterruptedError: continue
        if count <= 0: raise EOFError('private channel write failed')
        offset += count


def descriptor(value: str | None) -> int:
    if not isinstance(value, str) or not re.fullmatch(r'0|[1-9][0-9]{0,9}', value):
        raise ValueError('cleanup descriptor is not canonical')
    fd = int(value)
    if fd < 3 or fd > 2147483647:
        raise ValueError('cleanup descriptor overlaps stdio or exceeds its bound')
    return fd


def strict_object(pairs):
    obj = {}
    for key, value in pairs:
        if key in obj:
            raise ValueError('duplicate cleanup frame key')
        obj[key] = value
    return obj


def strict_json(data: bytes):
    if not 0 < len(data) <= MAX_BYTES:
        raise ValueError('cleanup frame exceeds bound')
    def constant(_): raise ValueError('nonfinite cleanup JSON')
    return json.loads(data, object_pairs_hook=strict_object, parse_constant=constant)


def decode(data: bytes, kind: str, expected=None) -> dict:
    obj = strict_json(data)
    if not isinstance(obj, dict) or set(obj) != KEYS or type(obj['version']) is not int or obj['version'] != 1 or obj['kind'] != kind:
        raise ValueError('cleanup frame shape/version/kind mismatch')
    if any(not isinstance(obj[key], str) or not re.fullmatch('[0-9a-f]{64}', obj[key]) for key in BINDINGS):
        raise ValueError('cleanup comparison binding is malformed')
    bindings = {key: obj[key] for key in BINDINGS}
    if expected is not None and bindings != expected:
        raise ValueError('cleanup original owner bindings changed')
    return bindings


def decode_init(data: bytes) -> CleanupInit:
    obj = strict_json(data)
    if (type(obj) is not dict or set(obj) != INIT_KEYS
            or type(obj['version']) is not int or obj['version'] != 2 or obj['kind'] != 'init'):
        raise ValueError('cleanup init-v2 frame shape/version/kind mismatch')
    if any(type(obj[key]) is not str or re.fullmatch(r'[0-9a-f]{64}', obj[key]) is None for key in BINDINGS):
        raise ValueError('cleanup original comparison binding is malformed')
    uuid = obj['uuid_v2_hex']
    if type(uuid) is not str or re.fullmatch(r'[0-9a-f]{32}', uuid) is None or uuid == '0' * 32:
        raise ValueError('cleanup selected UUID-v2 comparison is malformed')
    return CleanupInit(
        MappingProxyType({key: obj[key] for key in BINDINGS}), uuid,
        TreeBounds.from_contract(obj['gnu_bounds']),
        TreeBounds.from_contract(obj['driver_bounds']),
    )


def read_exact(fd: int, length: int) -> bytes:
    result = bytearray()
    while len(result) < length:
        try: chunk = os.read(fd, length - len(result))
        except InterruptedError: continue
        if not chunk: raise EOFError('private cleanup channel closed')
        result.extend(chunk)
    return bytes(result)


def read_frame(fd: int, kind: str, expected=None):
    size = struct.unpack('>I', read_exact(fd, 4))[0]
    if not 0 < size <= MAX_BYTES:
        raise ValueError('cleanup frame length exceeds bound')
    return decode(read_exact(fd, size), kind, expected)


def read_init_frame(fd: int) -> CleanupInit:
    size = struct.unpack('>I', read_exact(fd, 4))[0]
    if not 0 < size <= MAX_BYTES:
        raise ValueError('cleanup init-v2 frame length exceeds bound')
    return decode_init(read_exact(fd, size))


class CleanupChannel:
    def __init__(self, fd: int, session_fd: int):
        self._validate_socket(fd, session_fd)
        self.fd = fd
        self._phase = 'failed'
        self._preflight = None
        self._preflight_uuid = None
        self._install_init(read_init_frame(fd))

    @staticmethod
    def _validate_socket(fd, session_fd):
        if fd == session_fd or fd < 3 or fd > 2147483647 or session_fd in (1, 2):
            raise ValueError('cleanup channel overlaps session/stdout/stderr')
        if not stat.S_ISSOCK(os.fstat(fd).st_mode):
            raise ValueError('cleanup descriptor is not the inherited duplex socket')
    def _install_init(self, init):
        self.init = init
        self.bindings = self.init.bindings
        self.uuid_v2_hex = self.init.uuid_v2_hex
        self.gnu_bounds = self.init.gnu_bounds
        self.driver_bounds = self.init.driver_bounds
        self.authorized = False
        self.terminal = False
        self._phase = 'initialized'

    @classmethod
    def preflight(cls, fd: int, session_fd: int):
        """Explicit inert new mode; ordinary init-first entry remains unchanged.

        Daemon owns the original deadline/termination. These blocking child
        reads/writes do not create a timer or attest native-call interruption.
        """
        cls._validate_socket(fd, session_fd)
        value = cls.__new__(cls)
        value.fd = fd; value._phase = 'created'; value._preflight = None
        value._preflight_uuid = None
        value.authorized = False; value.terminal = False
        return value

    def read_preflight(self):
        previous, self._phase = self._phase, 'failed'
        if previous != 'created': raise RuntimeError('preflight read reused or wrong phase')
        self._preflight = decode_preflight(_read_body(self.fd))
        self._phase = 'preflight-read'
        return self._preflight

    def report_preflight(self, observation):
        previous, self._phase = self._phase, 'failed'
        if previous != 'preflight-read' or self._preflight is None:
            raise RuntimeError('preflight report premature or reused')
        request = self._preflight.values
        body = json.dumps(dict(version=3, kind='preflight-result',
            bindings=request['bindings'], challenge=request['challenge'], observation=observation),
            sort_keys=True, separators=(',', ':'), allow_nan=False).encode('utf-8')
        self._preflight_uuid = decode_preflight_result(body, self._preflight)['uuid_v2_hex']
        _write_body(self.fd, body)
        self._phase = 'preflight-reported'  # SAME socket retained; no EOF here

    def accept_init(self):
        previous, self._phase = self._phase, 'failed'
        if previous != 'preflight-reported' or self._preflight is None:
            raise RuntimeError('preflight init premature or reused')
        init = read_init_frame(self.fd)  # result replay here fails strict init-v2
        request = self._preflight.values
        if (dict(init.bindings) != request['bindings']
                or init.uuid_v2_hex != self._preflight_uuid
                or init.gnu_bounds != TreeBounds.from_contract(request['inputs']['gnu_bounds'])
                or init.driver_bounds != TreeBounds.from_contract(request['inputs']['driver_bounds'])):
            raise ValueError('preflight init changed original bindings/bounds')
        self._install_init(init)

    def authorize_shutdown(self):
        previous, self._phase = self._phase, 'failed'
        if previous != 'initialized' or self.terminal or self.authorized:
            raise RuntimeError('cleanup shutdown occurrence already consumed')
        read_frame(self.fd, 'shutdown', self.bindings)
        self.authorized = True
        self._phase = 'shutdown-authorized'

    def observe(self, observation):
        # CloseObservation data remains private; matching counts cannot grant
        # teardown. Lifecycle invokes only after complete retirement and fence.
        previous, self._phase = self._phase, 'failed'
        if previous != 'shutdown-authorized' or not self.authorized or self.terminal:
            raise RuntimeError('no authorized one-use cleanup shutdown')
        if observation.selected_device != 0:
            raise RuntimeError('cleanup product ordinal differs')
        self.terminal = True  # partial write may never retry or manufacture ACK
        data = json.dumps(dict(self.bindings, version=1, kind='retired'),
                          sort_keys=True, separators=(',', ':')).encode('utf-8')
        frame = struct.pack('>I', len(data)) + data
        offset = 0
        try:
            while offset < len(frame):
                try: count = os.write(self.fd, frame[offset:])
                except InterruptedError: continue
                if count <= 0: raise EOFError('cleanup channel write failed')
                offset += count
        finally:
            os.close(self.fd)  # retired then EOF; D still independently reaps group
