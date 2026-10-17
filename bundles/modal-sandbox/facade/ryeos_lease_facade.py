# ryeos:signed:2026-10-15T20:11:12Z:d30da7730b2ab3de94dab818b4b59a6b66ed91e417791850dcdf6e32b3d3cbe8:HbEvXvAuNW42oJNbsCzWfJ4NXonuL0NohleEnf1PKL7uU8AjR//HLiqaOCtW5sYcdGWe/9isSE6kt5cXi0StAA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
"""RyeOS Modal lease facade: fixed HTTP routes over Modal sandboxes.

One ``asgi_app`` behind Modal proxy auth serves eight fixed POST paths. The only
request header read is ``Content-Length``, used to refuse oversized bodies
before they are read. Each body is a JSON object checked against one fixed
lease policy; anything else is refused by name. The facade never chooses an
image, command, GPU, secret, volume or port from a request and never returns
provider exception messages. Every response is a JSON object carrying the
SHA-256 of this file's exact bytes and the guest image identity.

Lease lifecycle inside the sandbox: the sandbox command is a fixed shell loop
that waits for the run release directory and then ``exec``s the uploaded guest
with the run-once stdin file as its stdin, so the guest's exit is the
sandbox's exit and its return code.
"""
from __future__ import annotations

import asyncio
import base64
import binascii
import hashlib
import json
import re
import time
from pathlib import Path
from typing import Any, Callable, Mapping

import modal

RESPONSE_SCHEMA = 'ryeos.modal_lease_facade.response.v1'
POLICY_SCHEMA = 'ryeos.modal_lease_policy.v2'
FACADE_APP_NAME = 'ryeos-lease-facade'
LEASE_APP_NAME = 'ryeos-lease-sandboxes'
FACADE_SHA256 = hashlib.sha256(Path(__file__).read_bytes()).hexdigest()
MODAL_CLIENT_VERSION = str(getattr(modal, '__version__', 'unknown'))

# Fixed lease policy.
MAX_ALLOW_RULES = 4
MIN_TIMEOUT_SECONDS = 60
MAX_TIMEOUT_SECONDS = 86_400
LEASE_CPU = 1.0
LEASE_MEMORY_MIB = 1024
GUEST_DIR = '/ryeos/guest'
GUEST_ENTRYPOINT = '/ryeos/guest/entrypoint'
RUN_STDIN = '/ryeos/run.stdin'
UPLOAD_CLAIM = '/ryeos/upload.claimed'
UPLOAD_DONE = '/ryeos/upload.done'
RUN_CLAIM = '/ryeos/run.claimed'
RUN_RELEASE = '/ryeos/run.go'
STAGE_DIR = '/ryeos/stage'
STAGE_CLAIM_DIR = '/ryeos/stage-claim'
LEASE_INIT = (
    f'while [ ! -d {RUN_RELEASE} ]; do sleep 1; done; '
    f'exec {GUEST_ENTRYPOINT} < {RUN_STDIN}'
)
LEASE_COMMAND = ('/bin/sh', '-c', LEASE_INIT)
GUEST_IMAGE_IDENTITY = f'modal.debian_slim(python_version=3.12)+mkdir -p {GUEST_DIR}'
MAX_UPLOAD_BYTES = 64 * 1024 * 1024
MAX_STDIN_BYTES = 64 * 1024
# Staged slots: opaque named files filled by sequential chunks and sealed by
# digest. These constants are part of the policy record, so the pinned policy
# digest covers them.
MAX_STAGE_SLOTS = 4
MAX_STAGE_CHUNK_BYTES = 32 * 1024 * 1024
MAX_STAGE_SLOT_BYTES = 4 * 1024 * 1024 * 1024 + 64 * 1024
MAX_BODY_BYTES = 64 * 1024
# Routes carrying base64 payloads get room for the encoded payload plus a
# small JSON frame; every other route is held to MAX_BODY_BYTES.
ROUTE_BODY_BYTES = {
    'upload': (MAX_UPLOAD_BYTES // 3 + 1) * 4 + 4096,
    'run_once': (MAX_STDIN_BYTES // 3 + 1) * 4 + 4096,
    'stage': (MAX_STAGE_CHUNK_BYTES // 3 + 1) * 4 + 4096,
}
# No valid request needs more than a handful of objects and arrays (create
# needs at most 7); base64 text contains neither bracket.
MAX_STRUCTURE_BYTES = 32
MAX_LIST_RESULTS = 256
TERMINATE_OBSERVE_SECONDS = 60

FACADE_TAG = 'ryeos_facade'
LEASE_SET_TAG = 'ryeos_lease_set'
LEASE_NAME_TAG = 'ryeos_lease_name'
POLICY_TAG = 'ryeos_policy'
UPLOAD_TAG = 'ryeos_upload'
RUN_TAG = 'ryeos_run'
STAGE_TAG_PREFIX = 'ryeos_stage_'
CLAIMED = 'claimed'

_LABEL = r'[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?'
_DNS_NAME = re.compile(rf'(?:{_LABEL}\.)+[a-z](?:[a-z0-9-]{{0,61}}[a-z0-9])?')
_LEASE_NAME = re.compile(r'[a-z0-9][a-z0-9-]{0,62}')
_SANDBOX_ID = re.compile(r'sb-[A-Za-z0-9]{1,64}')
_SHA256_HEX = re.compile(r'[0-9a-f]{64}')
_PRINTABLE_FIELD = re.compile(r'[A-Za-z0-9_]{1,32}')
_SLOT_NAME = re.compile(r'[a-z0-9-]{1,32}')
_DECIMAL = re.compile(r'[0-9]{1,12}')

# Request fields with a named refusal of their own, so that a caller probing
# for a wider policy learns exactly which door is closed.
_REFUSED_FIELDS = {
    'image': ('image_refused', 'the guest image is fixed by the facade'),
    'command': ('command_refused', 'the guest command is fixed by the facade'),
    'entrypoint': ('command_refused', 'the guest command is fixed by the facade'),
    'args': ('command_refused', 'the guest command is fixed by the facade'),
    'gpu': ('gpu_refused', 'GPU leases are not part of the fixed policy'),
    'secrets': ('secrets_refused', 'secrets are never attached to a lease'),
    'volumes': ('volumes_refused', 'volumes are never attached to a lease'),
    'cidr_allowlist': ('cidr_refused', 'egress is named by exact DNS host only'),
    'encrypted_ports': ('ports_refused', 'inbound ports are never opened'),
    'unencrypted_ports': ('ports_refused', 'inbound ports are never opened'),
    'cpu': ('resources_refused', 'cpu and memory are fixed by the facade'),
    'memory': ('resources_refused', 'cpu and memory are fixed by the facade'),
    'path': ('path_refused', 'the upload path is fixed by the facade'),
}

GUEST_IMAGE = modal.Image.debian_slim(python_version='3.12').run_commands(
    f'mkdir -p {GUEST_DIR}'
)
WEB_IMAGE = modal.Image.debian_slim(python_version='3.12')
app = modal.App(FACADE_APP_NAME)
_sleep = time.sleep
_monotonic = time.monotonic


class Refusal(Exception):
    def __init__(self, code: str, detail: str, **extra: Any):
        super().__init__(code)
        self.code = code
        self.detail = detail
        self.extra = extra


class _Absent(Exception):
    """A lookup in get/terminate found no sandbox."""


def _response(operation: str, status: int, **fields: Any) -> tuple[int, dict]:
    body = {
        'schema': RESPONSE_SCHEMA,
        'facade_sha256': FACADE_SHA256,
        'facade_app': FACADE_APP_NAME,
        'guest_image': GUEST_IMAGE_IDENTITY,
        'modal_client': MODAL_CLIENT_VERSION,
        'operation': operation,
    }
    body.update(fields)
    return status, body


def _refusal(operation: str, status: int, code: str, detail: str, **extra: Any) -> tuple[int, dict]:
    return _response(operation, status, ok=False, refusal=code, detail=detail, **extra)


def _matches(pattern: re.Pattern, value: Any) -> bool:
    return isinstance(value, str) and pattern.fullmatch(value) is not None


def _field_label(name: Any) -> str:
    return name if _matches(_PRINTABLE_FIELD, name) else '<unprintable>'


def _check_fields(body: Any, required: set[str], optional: frozenset[str] = frozenset()) -> None:
    if not isinstance(body, dict):
        raise Refusal('body_not_object', 'the request body must be a JSON object')
    for name in body:
        if name in _REFUSED_FIELDS:
            raise Refusal(*_REFUSED_FIELDS[name])
        if name not in required and name not in optional:
            raise Refusal('unknown_field', f"field '{_field_label(name)}' is not part of the fixed lease policy")
    for name in sorted(required):
        if name not in body:
            raise Refusal('missing_field', f"field '{name}' is required")


def _lease_name(value: Any, field: str = 'name') -> str:
    if not _matches(_LEASE_NAME, value):
        raise Refusal(f'invalid_{field}', f'{field} must match [a-z0-9][a-z0-9-]{{0,62}}')
    return value


def _sandbox_id(value: Any) -> str:
    if not _matches(_SANDBOX_ID, value):
        raise Refusal('invalid_sandbox_id', 'sandbox_id must be a Modal sandbox id (sb-...)')
    return value


def _host(value: Any) -> str:
    if isinstance(value, str) and '*' in value:
        raise Refusal('wildcard_host_refused', 'wildcard hosts are refused; send exact DNS names')
    if not isinstance(value, str) or len(value) > 253 or not _matches(_DNS_NAME, value):
        raise Refusal('invalid_host', 'host must be an exact lowercase DNS name')
    return value


def parse_network(value: Any) -> dict:
    """Return Sandbox.create keyword arguments for one fixed network policy."""
    if not isinstance(value, dict):
        raise Refusal('invalid_network', 'network must be an object')
    kind = value.get('type')
    if kind == 'block-network':
        _check_fields(value, {'type'})
        return {'block_network': True}
    if kind != 'domain-allow-list':
        raise Refusal('invalid_network_type', "network type must be 'block-network' or 'domain-allow-list'")
    _check_fields(value, {'type', 'rules'})
    rules = value['rules']
    if not isinstance(rules, list) or not rules:
        raise Refusal('empty_allow_list', "an allow list needs at least one rule; use 'block-network' for none")
    if len(rules) > MAX_ALLOW_RULES:
        raise Refusal('too_many_rules', f'at most {MAX_ALLOW_RULES} allow-list rules')
    hosts: list[str] = []
    for rule in rules:
        if not isinstance(rule, dict):
            raise Refusal('invalid_rule', 'each rule must be an object')
        if 'port' in rule:
            raise Refusal('port_refused', 'rules carry no port; egress is TLS on 443 only')
        _check_fields(rule, {'protocol', 'host'})
        if rule['protocol'] == 'http':
            raise Refusal('plain_http_refused', 'plain http rules are refused; only https is accepted')
        if rule['protocol'] != 'https':
            raise Refusal('non_https_rule_refused', 'only https rules are accepted')
        host = _host(rule['host'])
        if host in hosts:
            raise Refusal('duplicate_host', 'each host may appear once')
        hosts.append(host)
    return {'block_network': False, 'outbound_domain_allowlist': hosts}


def _timeout(value: Any) -> int:
    if (
        isinstance(value, bool)
        or not isinstance(value, int)
        or not MIN_TIMEOUT_SECONDS <= value <= MAX_TIMEOUT_SECONDS
    ):
        raise Refusal(
            'timeout_out_of_range',
            f'timeout_seconds must be an integer in [{MIN_TIMEOUT_SECONDS}, {MAX_TIMEOUT_SECONDS}]',
        )
    return value


def _b64(value: Any, field: str, limit: int) -> bytes:
    # Size is checked on the encoded text before any decode.
    if not isinstance(value, str) or len(value) > (limit // 3 + 1) * 4:
        raise Refusal(f'{field}_too_large', f'{field} decodes to at most {limit} bytes')
    try:
        data = base64.b64decode(value, validate=True)
    except (binascii.Error, ValueError):
        raise Refusal('invalid_base64', f'{field} must be standard base64') from None
    if len(data) > limit:
        raise Refusal(f'{field}_too_large', f'{field} decodes to at most {limit} bytes')
    return data


def policy_record(lease_set: str, timeout: int, network: dict) -> dict:
    return {
        'schema': POLICY_SCHEMA,
        'lease_set': lease_set,
        'timeout_seconds': timeout,
        'cpu': LEASE_CPU,
        'memory_mib': LEASE_MEMORY_MIB,
        'block_network': network['block_network'],
        'outbound_domain_allowlist': sorted(network.get('outbound_domain_allowlist', [])),
        'image': GUEST_IMAGE_IDENTITY,
        'command': list(LEASE_COMMAND),
        'stage': {
            'max_slots': MAX_STAGE_SLOTS,
            'max_chunk_bytes': MAX_STAGE_CHUNK_BYTES,
            'max_slot_bytes': MAX_STAGE_SLOT_BYTES,
        },
    }


def policy_digest(record: dict) -> str:
    canonical = json.dumps(record, sort_keys=True, separators=(',', ':'))
    return hashlib.sha256(canonical.encode()).hexdigest()


def _is(error: Exception, name: str) -> bool:
    return type(error).__name__ == name


def _lease_app():
    return modal.App.lookup(LEASE_APP_NAME, create_if_missing=True)


def _by_id(sandbox_id: str, absent_ok: bool = False):
    try:
        return modal.Sandbox.from_id(sandbox_id)
    except Exception as error:
        if absent_ok and _is(error, 'NotFoundError'):
            raise _Absent() from None
        raise


def _by_name(name: str, absent_ok: bool = False):
    try:
        return modal.Sandbox.from_name(LEASE_APP_NAME, name)
    except Exception as error:
        if absent_ok and _is(error, 'NotFoundError'):
            raise _Absent() from None
        raise


def _observe(sandbox) -> dict:
    code = sandbox.poll()
    return {
        'sandbox_id': sandbox.object_id,
        'state': 'running' if code is None else 'terminated',
        'returncode': code,
    }


def _tagged(tags: Mapping[str, str]) -> bool:
    return (
        tags.get(FACADE_TAG) == '1'
        and _matches(_LEASE_NAME, tags.get(LEASE_NAME_TAG))
        and _matches(_LEASE_NAME, tags.get(LEASE_SET_TAG))
        and _matches(_SHA256_HEX, tags.get(POLICY_TAG))
    )


def _public_tags(tags: Mapping[str, str]) -> dict:
    keys = (LEASE_SET_TAG, LEASE_NAME_TAG, POLICY_TAG, UPLOAD_TAG, RUN_TAG)
    public = {key: tags[key] for key in keys if key in tags}
    public.update({key: value for key, value in tags.items() if key.startswith(STAGE_TAG_PREFIX)})
    return public


def _owned_by_id(sandbox) -> tuple[dict, bool]:
    """Ownership of a sandbox reached by id: the facade tags, and while it runs,
    the lease app's name index must resolve its tagged name back to it."""
    tags = dict(sandbox.get_tags())
    if not _tagged(tags):
        raise Refusal('not_facade_sandbox', 'the sandbox does not carry this facade\'s lease tags')
    if sandbox.poll() is not None:
        return tags, False
    try:
        named = modal.Sandbox.from_name(LEASE_APP_NAME, tags[LEASE_NAME_TAG])
    except Exception as error:
        if _is(error, 'NotFoundError'):
            named = None
        else:
            raise
    if named is None or named.object_id != sandbox.object_id:
        raise Refusal('not_lease_app_sandbox', 'the sandbox is not the lease app\'s sandbox of its tagged name')
    return tags, True


def _set_tags(sandbox, tags: dict, **updates: str) -> dict:
    merged = dict(tags)
    merged.update(updates)
    sandbox.set_tags(merged)
    return merged


def _run(sandbox, *argv: str) -> int:
    return sandbox.exec(*argv).wait()


def _step(sandbox, *argv: str) -> None:
    if _run(sandbox, *argv) != 0:
        raise Refusal('guest_step_failed', f'fixed step {argv[0]} failed')


def _read(sandbox, *argv: str) -> tuple[int, str]:
    """Run a fixed argv and return (exit code, stdout text)."""
    process = sandbox.exec(*argv)
    text = process.stdout.read()
    return process.wait(), text if isinstance(text, str) else bytes(text).decode('utf-8', 'replace')


def _guarded(operation: str, action: Callable[[], tuple[int, dict]]) -> tuple[int, dict]:
    try:
        return action()
    except Refusal as refusal:
        return _refusal(operation, 422, refusal.code, refusal.detail, **refusal.extra)
    except _Absent:
        return _response(operation, 200, ok=True, found=False)
    except Exception as error:
        # Class name only: provider messages are never copied into a response.
        return _response(operation, 502, ok=False, provider_error=type(error).__name__)


def handle_create(body: Any) -> tuple[int, dict]:
    def action():
        _check_fields(body, {'name', 'lease_set', 'timeout_seconds', 'network'})
        name = _lease_name(body['name'])
        lease_set = _lease_name(body['lease_set'], 'lease_set')
        timeout = _timeout(body['timeout_seconds'])
        network = parse_network(body['network'])
        record = policy_record(lease_set, timeout, network)
        digest = policy_digest(record)
        try:
            sandbox = modal.Sandbox.create(
                *LEASE_COMMAND,
                app=_lease_app(),
                image=GUEST_IMAGE,
                name=name,
                timeout=timeout,
                cpu=LEASE_CPU,
                memory=LEASE_MEMORY_MIB,
                **network,
            )
        except Exception as error:
            if not _is(error, 'AlreadyExistsError'):
                raise
            # Found by name in the lease app: facade-created by construction.
            # If it ended in between, this lookup's NotFoundError is a 502 and
            # the caller retries create.
            existing = _by_name(name)
            tags = dict(existing.get_tags())
            observed = _observe(existing)
            if not _tagged(tags):
                raise Refusal(
                    'name_taken_untagged',
                    'a lease of this name exists without its tags; a concurrent create may still be '
                    'tagging it: get it by name and retry, and terminate it by name only if it stays untagged',
                    name=name, sandbox_id=observed['sandbox_id'],
                ) from None
            if tags[LEASE_NAME_TAG] != name:
                raise Refusal(
                    'name_taken_tag_mismatch',
                    'the lease of this name carries another lease name tag',
                    name=name, sandbox_id=observed['sandbox_id'],
                ) from None
            if tags[POLICY_TAG] != digest:
                raise Refusal(
                    'name_taken_policy_mismatch',
                    'a lease of this name exists with a different policy',
                    name=name, sandbox_id=observed['sandbox_id'], stored_policy_sha256=tags[POLICY_TAG],
                    requested_policy_sha256=digest,
                ) from None
            return _response(
                'create', 200, ok=True, created=False, name_taken=True, name=name, policy_sha256=digest,
                policy=record, tags=_public_tags(tags), **observed,
            )
        tags = {
            FACADE_TAG: '1', LEASE_SET_TAG: lease_set, LEASE_NAME_TAG: name, POLICY_TAG: digest,
        }
        try:
            sandbox.set_tags(tags)
        except Exception:
            # An untagged lease is reachable only by name; do not leave one running.
            sandbox.terminate()
            raise
        return _response(
            'create', 200, ok=True, created=True, name=name, policy_sha256=digest, policy=record,
            tags=_public_tags(tags), **_observe(sandbox),
        )

    return _guarded('create', action)


def _selector(body: Any) -> tuple[str, str]:
    _check_fields(body, set(), frozenset({'sandbox_id', 'name'}))
    if ('sandbox_id' in body) == ('name' in body):
        raise Refusal('ambiguous_selector', "send exactly one of 'sandbox_id' or 'name'")
    if 'sandbox_id' in body:
        return 'sandbox_id', _sandbox_id(body['sandbox_id'])
    return 'name', _lease_name(body['name'])


def _select(kind: str, value: str):
    """Return (sandbox, tags, tagged, name_verified) for get and terminate."""
    if kind == 'sandbox_id':
        sandbox = _by_id(value, absent_ok=True)
        tags, verified = _owned_by_id(sandbox)
        return sandbox, tags, True, verified
    # Found by name in the lease app: facade-created by construction, even if
    # the function that created it died before tagging it.
    sandbox = _by_name(value, absent_ok=True)
    tags = dict(sandbox.get_tags())
    return sandbox, tags, _tagged(tags), True


def handle_get(body: Any) -> tuple[int, dict]:
    def action():
        sandbox, tags, tagged, verified = _select(*_selector(body))
        return _response(
            'get', 200, ok=True, found=True, tagged=tagged, name_verified=verified,
            tags=_public_tags(tags), **_observe(sandbox),
        )

    return _guarded('get', action)


def handle_terminate(body: Any) -> tuple[int, dict]:
    def action():
        sandbox, tags, tagged, _ = _select(*_selector(body))
        already_terminal = sandbox.poll() is not None
        if not already_terminal:
            sandbox.terminate()
            deadline = _monotonic() + TERMINATE_OBSERVE_SECONDS
            while sandbox.poll() is None and _monotonic() < deadline:
                _sleep(1)
        observed = _observe(sandbox)
        if observed['state'] != 'terminated':
            return _response(
                'terminate', 502, ok=False, provider_error='terminal_state_not_observed', **observed
            )
        return _response(
            'terminate', 200, ok=True, found=True, tagged=tagged, already_terminal=already_terminal,
            tags=_public_tags(tags), **observed,
        )

    return _guarded('terminate', action)


def _live_owned(sandbox_id: str):
    sandbox = _by_id(sandbox_id)
    tags, _ = _owned_by_id(sandbox)
    if sandbox.poll() is not None:
        raise Refusal('sandbox_terminated', 'the sandbox has already terminated')
    return sandbox, tags


def handle_upload(body: Any) -> tuple[int, dict]:
    def action():
        _check_fields(body, {'sandbox_id', 'content_b64', 'sha256'})
        sandbox_id = _sandbox_id(body['sandbox_id'])
        expected = body['sha256']
        if not _matches(_SHA256_HEX, expected):
            raise Refusal('invalid_sha256', 'sha256 must be 64 lowercase hex digits')
        data = _b64(body['content_b64'], 'content', MAX_UPLOAD_BYTES)
        if hashlib.sha256(data).hexdigest() != expected:
            raise Refusal('sha256_mismatch', 'content does not match sha256')
        sandbox, tags = _live_owned(sandbox_id)
        # The claim is recorded twice: as a tag (outside the guest's reach) and
        # as an atomic directory create (serialises concurrent calls).
        if UPLOAD_TAG in tags or _run(sandbox, 'mkdir', UPLOAD_CLAIM) != 0:
            raise Refusal('upload_already_claimed', 'the one upload for this lease was already claimed')
        tags = _set_tags(sandbox, tags, **{UPLOAD_TAG: CLAIMED})
        with sandbox.open(GUEST_ENTRYPOINT, 'wb') as handle:
            handle.write(data)
        _step(sandbox, 'chmod', '0755', GUEST_ENTRYPOINT)
        _step(sandbox, 'mkdir', UPLOAD_DONE)
        _set_tags(sandbox, tags, **{UPLOAD_TAG: expected})
        return _response(
            'upload', 200, ok=True, sandbox_id=sandbox_id, path=GUEST_ENTRYPOINT,
            bytes=len(data), sha256=expected,
        )

    return _guarded('upload', action)


def _slot(value: Any) -> str:
    if not _matches(_SLOT_NAME, value):
        raise Refusal('invalid_slot', 'slot must match [a-z0-9-]{1,32}')
    return value


def _stage_file(slot: str) -> str:
    return f'{STAGE_DIR}/{slot}'


def _stage_sealed_dir(slot: str) -> str:
    return f'{STAGE_DIR}/{slot}.done'


def _stage_claims(slot: str) -> str:
    return f'{STAGE_CLAIM_DIR}/{slot}'


def _stage_sealing_dir(slot: str) -> str:
    return f'{STAGE_DIR}/{slot}.sealing'


def _refuse_after_run_claim(sandbox, tags: dict) -> None:
    if RUN_TAG in tags or _run(sandbox, 'test', '-d', RUN_CLAIM) == 0:
        raise Refusal('run_once_already_claimed', 'the one run for this lease was already claimed')


def _refuse_if_sealing(sandbox, slot: str) -> None:
    if _run(sandbox, 'test', '-d', _stage_sealed_dir(slot)) == 0:
        raise Refusal('slot_sealed', 'the slot is sealed and takes no more chunks')
    if _run(sandbox, 'test', '-d', _stage_sealing_dir(slot)) == 0:
        raise Refusal('slot_sealed', 'the slot is being sealed and takes no more chunks')


def _refuse_if_raced(sandbox, slot: str) -> None:
    """Fail closed when a seal or the run claim landed while a chunk was in flight."""
    if (
        _run(sandbox, 'test', '-d', _stage_sealed_dir(slot)) == 0
        or _run(sandbox, 'test', '-d', _stage_sealing_dir(slot)) == 0
        or _run(sandbox, 'test', '-d', RUN_CLAIM) == 0
    ):
        raise Refusal('guest_step_failed', 'the slot was sealed or the run claimed while the chunk was in flight')


def _started_slots(sandbox) -> list[str]:
    if _run(sandbox, 'test', '-d', STAGE_CLAIM_DIR) != 0:
        return []
    code, text = _read(sandbox, 'ls', '-1', STAGE_CLAIM_DIR)
    if code != 0:
        raise Refusal('guest_step_failed', 'fixed step ls failed')
    return sorted(line for line in text.split('\n') if line)


def _stage_size(sandbox, slot: str) -> int:
    path = _stage_file(slot)
    if _run(sandbox, 'test', '-f', path) != 0:
        return 0
    code, text = _read(sandbox, 'stat', '-c', '%s', path)
    if code != 0 or not _matches(_DECIMAL, text.strip()):
        raise Refusal('guest_step_failed', 'fixed step stat failed')
    return int(text.strip())


def handle_stage(body: Any) -> tuple[int, dict]:
    def action():
        _check_fields(body, {'sandbox_id', 'slot', 'offset', 'content_b64', 'chunk_sha256'})
        sandbox_id = _sandbox_id(body['sandbox_id'])
        slot = _slot(body['slot'])
        offset = body['offset']
        if isinstance(offset, bool) or not isinstance(offset, int) or not 0 <= offset <= MAX_STAGE_SLOT_BYTES:
            raise Refusal('invalid_offset', f'offset must be an integer in [0, {MAX_STAGE_SLOT_BYTES}]')
        expected = body['chunk_sha256']
        if not _matches(_SHA256_HEX, expected):
            raise Refusal('invalid_sha256', 'chunk_sha256 must be 64 lowercase hex digits')
        data = _b64(body['content_b64'], 'chunk', MAX_STAGE_CHUNK_BYTES)
        if not data:
            raise Refusal('chunk_empty', 'a chunk carries at least one byte')
        if hashlib.sha256(data).hexdigest() != expected:
            raise Refusal('chunk_sha256_mismatch', 'content does not match chunk_sha256')
        if offset + len(data) > MAX_STAGE_SLOT_BYTES:
            raise Refusal('slot_too_large', f'a slot holds at most {MAX_STAGE_SLOT_BYTES} bytes')
        sandbox, tags = _live_owned(sandbox_id)
        _step(sandbox, 'mkdir', '-p', STAGE_DIR, STAGE_CLAIM_DIR)
        _refuse_after_run_claim(sandbox, tags)
        _refuse_if_sealing(sandbox, slot)
        # A slot not yet started only starts at offset 0, checked before any
        # claim is created so a refused first chunk leaves nothing behind.
        started = _run(sandbox, 'test', '-d', _stage_claims(slot)) == 0
        if not started and offset != 0:
            raise Refusal('offset_mismatch', 'offset must equal the slot size so far', slot_bytes=0)
        fresh = False
        try:
            if not started and _run(sandbox, 'mkdir', _stage_claims(slot)) == 0:
                fresh = True
                # First chunk of this slot: take a slot of the lease's budget,
                # then re-count so concurrent starters cannot jointly exceed it.
                if len(_started_slots(sandbox)) > MAX_STAGE_SLOTS:
                    raise Refusal('too_many_slots', f'at most {MAX_STAGE_SLOTS} slots per lease')
            size = _stage_size(sandbox, slot)
            if size != offset:
                raise Refusal('offset_mismatch', 'offset must equal the slot size so far', slot_bytes=size)
            if _run(sandbox, 'mkdir', f'{_stage_claims(slot)}/c{offset}') != 0:
                raise Refusal('chunk_already_claimed', 'this chunk of the slot was already claimed')
        except Refusal:
            if fresh:
                _run(sandbox, 'rmdir', _stage_claims(slot))
            raise
        # A seal or run claim taken after the checks above must not be missed.
        _refuse_if_raced(sandbox, slot)
        with sandbox.open(_stage_file(slot), 'ab') as handle:
            handle.write(data)
        _refuse_if_raced(sandbox, slot)
        if _stage_size(sandbox, slot) != offset + len(data):
            raise Refusal('guest_step_failed', 'the slot size after the append is not the expected one')
        return _response(
            'stage', 200, ok=True, sandbox_id=sandbox_id, slot=slot, path=_stage_file(slot),
            offset=offset, bytes=len(data), slot_bytes=offset + len(data), sha256=expected,
        )

    return _guarded('stage', action)


def handle_stage_seal(body: Any) -> tuple[int, dict]:
    def action():
        _check_fields(body, {'sandbox_id', 'slot', 'total_bytes', 'sha256'})
        sandbox_id = _sandbox_id(body['sandbox_id'])
        slot = _slot(body['slot'])
        total = body['total_bytes']
        if isinstance(total, bool) or not isinstance(total, int) or not 1 <= total <= MAX_STAGE_SLOT_BYTES:
            raise Refusal('invalid_total_bytes', f'total_bytes must be an integer in [1, {MAX_STAGE_SLOT_BYTES}]')
        expected = body['sha256']
        if not _matches(_SHA256_HEX, expected):
            raise Refusal('invalid_sha256', 'sha256 must be 64 lowercase hex digits')
        sandbox, tags = _live_owned(sandbox_id)
        tag = f'{STAGE_TAG_PREFIX}{slot}'
        if _run(sandbox, 'test', '-d', _stage_sealed_dir(slot)) == 0:
            if tags.get(tag) != expected:
                raise Refusal('seal_digest_conflict', 'the slot is already sealed with another digest')
            size = _stage_size(sandbox, slot)
            if size != total:
                raise Refusal('seal_size_mismatch', 'total_bytes does not match the slot size', slot_bytes=size)
            return _response(
                'stage_seal', 200, ok=True, sandbox_id=sandbox_id, slot=slot, sealed=True,
                already_sealed=True, bytes=total, sha256=expected,
            )
        _refuse_after_run_claim(sandbox, tags)
        if _run(sandbox, 'test', '-d', _stage_claims(slot)) != 0:
            raise Refusal('slot_not_started', 'the slot has no chunks')
        # The seal claim stops new chunks; it is released if the seal is refused.
        if _run(sandbox, 'mkdir', _stage_sealing_dir(slot)) != 0:
            raise Refusal('slot_sealed', 'the slot is already being sealed')
        try:
            size = _stage_size(sandbox, slot)
            if size != total:
                raise Refusal('seal_size_mismatch', 'total_bytes does not match the slot size', slot_bytes=size)
            code, text = _read(sandbox, 'sha256sum', _stage_file(slot))
            digest = text.split(' ', 1)[0]
            if code != 0 or not _matches(_SHA256_HEX, digest):
                raise Refusal('guest_step_failed', 'fixed step sha256sum failed')
            if digest != expected:
                raise Refusal('seal_digest_mismatch', 'the slot content does not match sha256')
            _set_tags(sandbox, tags, **{tag: expected})
            _step(sandbox, 'mkdir', _stage_sealed_dir(slot))
        except Refusal:
            if _run(sandbox, 'test', '-d', _stage_sealed_dir(slot)) != 0:
                _run(sandbox, 'rmdir', _stage_sealing_dir(slot))
            raise
        return _response(
            'stage_seal', 200, ok=True, sandbox_id=sandbox_id, slot=slot, sealed=True,
            already_sealed=False, bytes=total, sha256=expected,
        )

    return _guarded('stage_seal', action)


def handle_run_once(body: Any) -> tuple[int, dict]:
    def action():
        _check_fields(body, {'sandbox_id', 'stdin_b64'})
        sandbox_id = _sandbox_id(body['sandbox_id'])
        stdin = _b64(body['stdin_b64'], 'stdin', MAX_STDIN_BYTES)
        sandbox, tags = _live_owned(sandbox_id)
        if RUN_TAG in tags:
            raise Refusal('run_once_already_claimed', 'the one run for this lease was already claimed')
        if not _matches(_SHA256_HEX, tags.get(UPLOAD_TAG)) or _run(sandbox, 'test', '-d', UPLOAD_DONE) != 0:
            raise Refusal('upload_not_complete', 'run-once needs a completed upload first')
        for slot in _started_slots(sandbox):
            if _run(sandbox, 'test', '-d', _stage_sealed_dir(slot)) != 0:
                raise Refusal('slot_not_sealed', 'every started slot must be sealed before run-once', slot=slot)
        if _run(sandbox, 'mkdir', RUN_CLAIM) != 0:
            raise Refusal('run_once_already_claimed', 'the one run for this lease was already claimed')
        tags = _set_tags(sandbox, tags, **{RUN_TAG: CLAIMED})
        with sandbox.open(RUN_STDIN, 'wb') as handle:
            handle.write(stdin)
        _step(sandbox, 'mkdir', RUN_RELEASE)
        digest = hashlib.sha256(stdin).hexdigest()
        _set_tags(sandbox, tags, **{RUN_TAG: digest})
        return _response(
            'run_once', 200, ok=True, released=True, argv=[GUEST_ENTRYPOINT], stdin_path=RUN_STDIN,
            stdin_bytes=len(stdin), stdin_sha256=digest, **_observe(sandbox),
        )

    return _guarded('run_once', action)


def handle_list(body: Any) -> tuple[int, dict]:
    def action():
        _check_fields(body, {'lease_set'})
        lease_set = _lease_name(body['lease_set'], 'lease_set')
        ids: list[str] = []
        untagged: list[str] = []
        truncated = False
        # Every sandbox in the lease app is listed and filtered here, so an
        # untagged lease stays visible.
        for sandbox in modal.Sandbox.list(app_id=_lease_app().app_id):
            tags = dict(sandbox.get_tags())
            if _tagged(tags):
                if tags[LEASE_SET_TAG] != lease_set:
                    continue
                bucket = ids
            else:
                bucket = untagged
            if len(ids) + len(untagged) == MAX_LIST_RESULTS:
                truncated = True
                break
            bucket.append(sandbox.object_id)
        return _response(
            'list', 200, ok=True, lease_set=lease_set, sandbox_ids=ids,
            untagged_sandbox_ids=untagged, truncated=truncated,
        )

    return _guarded('list', action)


HANDLERS: Mapping[str, Callable[[Any], tuple[int, dict]]] = {
    'create': handle_create,
    'get': handle_get,
    'terminate': handle_terminate,
    'upload': handle_upload,
    'stage': handle_stage,
    'stage_seal': handle_stage_seal,
    'run_once': handle_run_once,
    'list': handle_list,
}
ROUTES = {
    '/v1/create': 'create',
    '/v1/get': 'get',
    '/v1/terminate': 'terminate',
    '/v1/upload': 'upload',
    '/v1/stage': 'stage',
    '/v1/stage-seal': 'stage_seal',
    '/v1/run-once': 'run_once',
    '/v1/list': 'list',
}


def _no_duplicates(pairs: list) -> dict:
    out: dict = {}
    for key, value in pairs:
        if key in out:
            raise ValueError('duplicate key')
        out[key] = value
    return out


def _content_length(headers) -> int | None:
    values = [value for key, value in headers if key.lower() == b'content-length']
    if len(values) != 1:
        return None
    text = values[0].decode('latin-1')
    return int(text) if _matches(_DECIMAL, text) else None


async def _serve(scope, receive) -> tuple[int, dict]:
    operation = ROUTES.get(scope.get('path'))
    if operation is None:
        return _refusal('unknown', 404, 'unknown_route', 'no such route')
    if scope.get('method') != 'POST':
        return _refusal(operation, 405, 'method_not_allowed', 'routes accept POST only')
    length = _content_length(scope.get('headers', []))
    if length is None:
        return _refusal(operation, 411, 'content_length_required', 'send one decimal Content-Length')
    cap = ROUTE_BODY_BYTES.get(operation, MAX_BODY_BYTES)
    if length > cap:
        return _refusal(operation, 413, 'body_too_large', f'bodies on this route are at most {cap} bytes')
    chunks: list[bytes] = []
    received = 0
    while True:
        message = await receive()
        if message['type'] != 'http.request':
            return _refusal(operation, 400, 'request_aborted', 'the request ended early')
        chunk = message.get('body', b'')
        received += len(chunk)
        if received > length:
            return _refusal(operation, 413, 'body_too_large', 'body exceeds its Content-Length')
        chunks.append(chunk)
        if not message.get('more_body', False):
            break
    if received != length:
        return _refusal(operation, 400, 'body_length_mismatch', 'body is shorter than its Content-Length')
    data = b''.join(chunks)
    del chunks
    if data.count(b'{') + data.count(b'[') > MAX_STRUCTURE_BYTES:
        return _refusal(operation, 400, 'too_many_structures', 'the body has more objects and arrays than any request needs')
    try:
        parsed = json.loads(data, object_pairs_hook=_no_duplicates)
    except (ValueError, UnicodeDecodeError, RecursionError):
        return _refusal(operation, 400, 'invalid_json', 'the body must be one JSON object with unique keys')
    del data
    return await asyncio.to_thread(HANDLERS[operation], parsed)


async def lease_asgi(scope, receive, send) -> None:
    if scope['type'] == 'lifespan':
        while True:
            message = await receive()
            if message['type'] == 'lifespan.startup':
                await send({'type': 'lifespan.startup.complete'})
            elif message['type'] == 'lifespan.shutdown':
                await send({'type': 'lifespan.shutdown.complete'})
                return
    if scope['type'] != 'http':
        return
    status, body = await _serve(scope, receive)
    payload = json.dumps(body, sort_keys=True, separators=(',', ':')).encode()
    await send({
        'type': 'http.response.start',
        'status': status,
        'headers': [(b'content-type', b'application/json'), (b'content-length', str(len(payload)).encode())],
    })
    await send({'type': 'http.response.body', 'body': payload})


@app.function(image=WEB_IMAGE, timeout=300, memory=1024)
@modal.asgi_app(label='ryeos-lease', requires_proxy_auth=True)
def web():
    return lease_asgi
