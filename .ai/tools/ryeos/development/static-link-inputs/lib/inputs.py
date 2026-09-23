# ryeos:signed:2026-09-22T04:12:04Z:500d509e0dc69c7805c2645a860f95a9438d381421fc3881ccac984d8d8b2b0f:NoMy4gbs7npON0/AA8l7beUtBZyqkknYZWBIgAYygMKS+N6+AjVqcds/IvBF1GlqKWQDRjSndVkcnRLT5DVvDg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
"""Shared bounded static-input contract parser; no acquisition or execution."""
import json
from pathlib import PurePosixPath
import re

MAXIMUM_BYTES = 32 * 1024 * 1024


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f'duplicate contract key: {key}')
        result[key] = value
    return result


def relative_path(value):
    if (not isinstance(value, str) or not value or value.startswith('/')
            or any(part in ('', '.', '..') for part in value.split('/'))
            or '\\' in value or '\x00' in value):
        raise ValueError(f'unsafe relative path: {value!r}')
    return value


def read_contract(path):
    if path.is_symlink() or not path.is_file() or path.stat().st_size > 65536:
        raise ValueError('unsafe or oversized static-link contract')
    text = path.read_text()
    if text.startswith('# ryeos:signed:'):
        text = text.split('\n', 1)[1]
    contract = json.loads(text, object_pairs_hook=unique_object)
    required = {'schema', 'publisher_image', 'target', 'inputs'}
    metadata = {'category', 'name', 'version', 'description'}
    if (not isinstance(contract, dict) or not required <= set(contract)
            or set(contract) - required - metadata):
        raise ValueError('unexpected static-link contract fields')
    if contract.get('schema') != 'ryeos.development.static_link_inputs.v1':
        raise ValueError('unsupported static-link contract schema')
    if not re.fullmatch(r'[A-Za-z0-9./:_-]+@sha256:[0-9a-f]{64}', contract.get('publisher_image', '')):
        raise ValueError('publisher must be digest-pinned')
    if contract.get('target') != 'x86_64-unknown-linux-gnu':
        raise ValueError('unsupported static-link target')
    inputs = contract.get('inputs')
    if not isinstance(inputs, dict) or not 1 <= len(inputs) <= 64:
        raise ValueError('invalid static-link input set')
    sources = set()
    total = 0
    for destination, item in inputs.items():
        relative_path(destination)
        if not isinstance(item, dict) or set(item) != {'source', 'bytes', 'mode', 'sha256'}:
            raise ValueError('unexpected static-link input fields')
        source = item['source']
        if not isinstance(source, str) or not source.startswith('/'):
            raise ValueError('input source must be absolute')
        relative_path(source[1:])
        if source in sources:
            raise ValueError('duplicate input source')
        sources.add(source)
        if type(item['bytes']) is not int or not 0 < item['bytes'] <= MAXIMUM_BYTES:
            raise ValueError('invalid input size')
        if type(item['mode']) is not int or not 0 <= item['mode'] <= 0o777:
            raise ValueError('invalid input mode')
        if not re.fullmatch('[0-9a-f]{64}', item['sha256']):
            raise ValueError('invalid input digest')
        total += item['bytes']
    if total > MAXIMUM_BYTES:
        raise ValueError('static-link input set exceeds byte limit')
    destinations = set(inputs)
    if any(str(parent) in destinations for name in inputs for parent in PurePosixPath(name).parents):
        raise ValueError('input destinations overlap')
    return contract
