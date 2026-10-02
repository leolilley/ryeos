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


@dataclass(frozen=True)
class CleanupInit:
    """Typed startup comparisons, never original owner or device authority."""
    bindings: Mapping[str, str]
    uuid_v2_hex: str
    gnu_bounds: TreeBounds
    driver_bounds: TreeBounds


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
        if fd == session_fd or fd < 3 or fd > 2147483647 or session_fd in (1, 2):
            raise ValueError('cleanup channel overlaps session/stdout/stderr')
        if not stat.S_ISSOCK(os.fstat(fd).st_mode):
            raise ValueError('cleanup descriptor is not the inherited duplex socket')
        self.fd = fd
        self.init = read_init_frame(fd)
        self.bindings = self.init.bindings
        self.uuid_v2_hex = self.init.uuid_v2_hex
        self.gnu_bounds = self.init.gnu_bounds
        self.driver_bounds = self.init.driver_bounds
        self.authorized = False
        self.terminal = False

    def authorize_shutdown(self):
        if self.terminal or self.authorized:
            raise RuntimeError('cleanup shutdown occurrence already consumed')
        read_frame(self.fd, 'shutdown', self.bindings)
        self.authorized = True

    def observe(self, observation):
        # CloseObservation data remains private; matching counts cannot grant
        # teardown. Lifecycle invokes only after complete retirement and fence.
        if not self.authorized or self.terminal:
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
