"""Read-only native qualification input for the selected ordinal-zero producer.

NOT an admitted observer or launch capability. The caller must already retain an
independent same-node GPU UUID-v2 observation joined to Lillux's exact selected
character-device authorities. There is no such qualified node observer in the
current resource pool. This separate diagnostic is not invoked by runtime.py:
the source-wired worker instead passes strict init-v2 comparison data to
Lifecycle.install, which checks the actual context-creating CUdevice handle.
Neither path admits the supplied comparison. This helper loads neither
bindings nor libraries and creates no CUDA context or allocation.
"""
from __future__ import annotations

import ctypes
import re
from dataclasses import dataclass


class DeviceBindingError(RuntimeError):
    pass


@dataclass(frozen=True)
class DeviceBindingObservation:
    visible_device_count: int
    selected_ordinal: int
    uuid_v2_hex: str


def qualify_ordinal_zero(*, bindings, expected_uuid_v2_hex: str) -> DeviceBindingObservation:
    """Query an already selected driver, before lifecycle/consumer installation.

    expected_uuid_v2_hex is comparison data, NEVER authority. A copyable expected
    UUID or matching result does not create the independently observed node fact,
    retained same-device descriptor join, lease or authenticated cleanup evidence.
    Native qualification caller owns original deadline/process/channel/retirement.
    No fallback to the older UUID API or PCI/name/ordinal identity is permitted.
    """
    if (not isinstance(expected_uuid_v2_hex, str)
            or re.fullmatch('[0-9a-f]{32}', expected_uuid_v2_hex) is None
            or expected_uuid_v2_hex == '0' * 32):
        raise DeviceBindingError('missing canonical independently observed UUID-v2 comparison')
    if getattr(bindings, '_ryeos_lifecycle', False):
        raise DeviceBindingError('device qualifier must precede lifecycle installation')
    # Resolve the entire finite query surface before touching the native driver.
    try:
        init = bindings.cuInit
        get_count = bindings.cuDeviceGetCount
        get_device = bindings.cuDeviceGet
        get_uuid = bindings.cuDeviceGetUuid_v2
        device_type = bindings.CUdevice
        uuid_type = bindings.CUuuid
    except AttributeError as error:
        raise DeviceBindingError('selected driver bindings lack required UUID-v2 query') from error
    if ctypes.sizeof(uuid_type) != 16:
        raise DeviceBindingError('selected UUID-v2 binding layout differs')

    def checked(name, function, *args):
        try:
            status = function(*args)
        except Exception as error:
            raise DeviceBindingError(f'unknown native device query {name}') from error
        if type(status) is not int or status != 0:
            raise DeviceBindingError(f'native device query {name} failed')

    checked('cuInit', init, 0)
    count = ctypes.c_int(-1)
    checked('cuDeviceGetCount', get_count, ctypes.byref(count))
    if count.value != 1:
        raise DeviceBindingError('selected product requires exactly one visible CUDA device')
    device = device_type()
    checked('cuDeviceGet', get_device, ctypes.byref(device), 0)
    uuid = uuid_type()
    checked('cuDeviceGetUuid_v2', get_uuid, ctypes.byref(uuid), device)
    # CUuuid contains c_char[16]; .bytes may terminate at an embedded NUL.
    # Copy exactly the complete native structure, including zero-valued octets.
    observed = ctypes.string_at(ctypes.byref(uuid), 16).hex()
    if observed != expected_uuid_v2_hex:
        raise DeviceBindingError('ordinal zero differs from independently observed device UUID-v2')
    return DeviceBindingObservation(1, 0, observed)
