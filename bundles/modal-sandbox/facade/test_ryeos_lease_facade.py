"""Offline tests for the Modal lease facade with the Modal SDK replaced."""
from __future__ import annotations

import asyncio
import base64
import contextlib
import hashlib
import importlib.util
import json
import sys
import types
import unittest
from pathlib import Path

FACADE_PATH = Path(__file__).with_name('ryeos_lease_facade.py')
FACADE_SHA256 = hashlib.sha256(FACADE_PATH.read_bytes()).hexdigest()


class NotFoundError(Exception):
    pass


class AlreadyExistsError(Exception):
    pass


class ProviderBoom(Exception):
    pass


class FakeProcess:
    def __init__(self, code):
        self.code = code

    def wait(self):
        return self.code


class FakeFile(contextlib.AbstractContextManager):
    def __init__(self, sandbox, path):
        self.sandbox, self.path = sandbox, path

    def write(self, data):
        self.sandbox.files[self.path] = data

    def __exit__(self, *exc):
        return False


class FakeSandbox:
    def __init__(self, provider, name, kwargs):
        provider.counter += 1
        self.object_id = f'sb-fake{provider.counter}'
        self.name = name
        self.kwargs = kwargs
        self.tags = {}
        self.returncode = None
        self.dirs = set()
        self.files = {}
        self.execs = []
        self.tag_history = []
        self.fail_set_tags = False
        self.stuck = False
        self.fail_exec = set()

    def poll(self):
        return self.returncode

    def get_tags(self):
        return dict(self.tags)

    def set_tags(self, tags):
        if self.fail_set_tags:
            raise ProviderBoom('secret-looking provider text')
        self.tags = dict(tags)
        self.tag_history.append(dict(tags))

    def terminate(self):
        if not self.stuck and self.returncode is None:
            self.returncode = 137

    def exec(self, *argv):
        self.execs.append(argv)
        code = 0
        if argv[0] in self.fail_exec:
            code = 1
        elif argv[0] == 'mkdir':
            code = 1 if argv[1] in self.dirs else 0
            self.dirs.add(argv[1])
        elif argv[:2] == ('test', '-d'):
            code = 0 if argv[2] in self.dirs else 1
        return FakeProcess(code)

    def open(self, path, mode):
        assert mode == 'wb'
        return FakeFile(self, path)


class FakeProvider:
    def __init__(self):
        self.counter = 0
        self.by_id = {}
        self.by_name = {}
        self.create_calls = []
        self.list_calls = []
        self.fail_next_set_tags = False
        self.create_error = None

    def create(self, *argv, app=None, name=None, **kwargs):
        self.create_calls.append((argv, app, name, kwargs))
        if self.create_error is not None:
            raise self.create_error
        live = self.by_name.get(name)
        if live is not None and live.returncode is None:
            raise AlreadyExistsError('name taken')
        sandbox = FakeSandbox(self, name, dict(kwargs, argv=argv))
        sandbox.fail_set_tags = self.fail_next_set_tags
        self.by_id[sandbox.object_id] = sandbox
        self.by_name[name] = sandbox
        return sandbox

    def add_foreign(self, name=None, tags=None):
        sandbox = FakeSandbox(self, name, {})
        sandbox.tags = dict(tags or {})
        self.by_id[sandbox.object_id] = sandbox
        if name is not None:
            self.by_name[name] = sandbox
        return sandbox

    def from_id(self, sandbox_id):
        if sandbox_id not in self.by_id:
            raise NotFoundError(sandbox_id)
        return self.by_id[sandbox_id]

    def from_name(self, app_name, name):
        assert app_name == 'ryeos-lease-sandboxes'
        sandbox = self.by_name.get(name)
        if sandbox is None or sandbox.returncode is not None:
            raise NotFoundError(name)
        return sandbox

    def list(self, **kwargs):
        self.list_calls.append(kwargs)
        return [s for s in self.by_id.values() if s.returncode is None]


class FakeImage:
    def __init__(self, steps=()):
        self.steps = tuple(steps)

    @classmethod
    def debian_slim(cls, python_version):
        return cls((('debian_slim', python_version),))

    def run_commands(self, *commands):
        return FakeImage(self.steps + (('run_commands', commands),))


def load_facade():
    provider = FakeProvider()
    record = {'functions': {}, 'asgi': []}

    class FakeApp:
        def __init__(self, name):
            self.name = name

        def function(self, **kwargs):
            def register(fn):
                record['functions'][fn.__name__] = kwargs
                return fn
            return register

        @staticmethod
        def lookup(name, create_if_missing=False):
            return types.SimpleNamespace(name=name, app_id='ap-leases', create_if_missing=create_if_missing)

    def asgi_app(**kwargs):
        def register(fn):
            record['asgi'].append((fn.__name__, kwargs))
            return fn
        return register

    modal = types.ModuleType('modal')
    modal.__version__ = '1.0.0-fake'
    modal.App = FakeApp
    modal.Image = FakeImage
    modal.asgi_app = asgi_app
    modal.Sandbox = types.SimpleNamespace(
        create=provider.create, from_id=provider.from_id,
        from_name=provider.from_name, list=provider.list,
    )
    saved = sys.modules.get('modal')
    sys.modules['modal'] = modal
    try:
        spec = importlib.util.spec_from_file_location('ryeos_lease_facade_under_test', FACADE_PATH)
        facade = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(facade)
    finally:
        if saved is None:
            sys.modules.pop('modal', None)
        else:
            sys.modules['modal'] = saved
    facade._sleep = lambda seconds: None
    return facade, provider, record


def b64(data: bytes) -> str:
    return base64.b64encode(data).decode()


def allow(*hosts):
    return {'type': 'domain-allow-list', 'rules': [{'protocol': 'https', 'host': h} for h in hosts]}


def create_body(**overrides):
    body = {
        'name': 'lease-1',
        'lease_set': 'node-a',
        'timeout_seconds': 3600,
        'network': allow('controller.example.com'),
    }
    body.update(overrides)
    return body


def call_asgi(facade, path, body=b'', method='POST', headers=None, chunks=None):
    if headers is None:
        headers = [(b'content-length', str(len(body)).encode())]
    messages = [{'type': 'http.request', 'body': c, 'more_body': i < len(chunks) - 1}
                for i, c in enumerate(chunks)] if chunks else [{'type': 'http.request', 'body': body}]
    sent = []

    async def receive():
        return messages.pop(0) if messages else {'type': 'http.disconnect'}

    async def send(message):
        sent.append(message)

    scope = {'type': 'http', 'path': path, 'method': method, 'headers': headers}
    asyncio.run(facade.lease_asgi(scope, receive, send))
    return sent[0]['status'], json.loads(sent[1]['body'])


class FacadeTest(unittest.TestCase):
    def setUp(self):
        self.facade, self.provider, self.record = load_facade()

    def assertEnvelope(self, body, operation):
        self.assertEqual(body['schema'], 'ryeos.modal_lease_facade.response.v1')
        self.assertEqual(body['facade_sha256'], FACADE_SHA256)
        self.assertEqual(body['facade_app'], 'ryeos-lease-facade')
        self.assertEqual(body['guest_image'], 'modal.debian_slim(python_version=3.12)+mkdir -p /ryeos/guest')
        self.assertEqual(body['modal_client'], '1.0.0-fake')
        self.assertEqual(body['operation'], operation)
        json.dumps(body)

    def refused(self, handler, body, code):
        status, out = handler(body)
        self.assertEqual(status, 422, out)
        self.assertFalse(out['ok'])
        self.assertEqual(out['refusal'], code, out)
        self.assertEnvelope(out, out['operation'])
        return out

    def created(self, **overrides):
        status, out = self.facade.handle_create(create_body(**overrides))
        self.assertEqual(status, 200, out)
        return out

    def uploaded(self, data=b'\x7fELF guest', **overrides):
        out = self.created(**overrides)
        status, up = self.facade.handle_upload(
            {'sandbox_id': out['sandbox_id'], 'content_b64': b64(data), 'sha256': hashlib.sha256(data).hexdigest()})
        self.assertEqual(status, 200, up)
        return out, self.provider.by_id[out['sandbox_id']]

    # Deployment shape

    def test_one_proxy_authenticated_asgi_app(self):
        self.assertEqual(self.record['asgi'], [('web', {'label': 'ryeos-lease', 'requires_proxy_auth': True})])
        self.assertEqual(self.record['functions']['web']['memory'], 1024)
        self.assertEqual(self.record['functions']['web']['timeout'], 300)
        self.assertEqual(set(self.facade.ROUTES), {
            '/v1/create', '/v1/get', '/v1/terminate', '/v1/upload', '/v1/run-once', '/v1/list'})

    def test_facade_reads_no_credentials(self):
        code = FACADE_PATH.read_text().split('"""', 2)[2]
        for needle in ('Modal-Key', 'Modal-Secret', 'authorization', 'environ', 'modal.Secret', 'fastapi'):
            self.assertFalse(needle in code, needle)

    # HTTP layer

    def test_asgi_round_trip(self):
        body = json.dumps(create_body()).encode()
        status, out = call_asgi(self.facade, '/v1/create', body)
        self.assertEqual(status, 200)
        self.assertEnvelope(out, 'create')
        self.assertTrue(out['created'])

    def test_asgi_chunked_body(self):
        body = json.dumps({'lease_set': 'node-a'}).encode()
        status, out = call_asgi(self.facade, '/v1/list', body, chunks=[body[:5], body[5:]])
        self.assertEqual((status, out['ok']), (200, True))

    def test_asgi_refusals_use_envelope(self):
        big = str(self.facade.ROUTE_BODY_BYTES['upload'] + 1).encode()
        small = str(64 * 1024 + 1).encode()
        run_big = str(self.facade.ROUTE_BODY_BYTES['run_once'] + 1).encode()
        cases = [
            (('/v1/nope', b'{}'), {}, 404, 'unknown_route', 'unknown'),
            (('/v1/get', b'{}'), {'method': 'GET'}, 405, 'method_not_allowed', 'get'),
            (('/v1/get', b'{}'), {'headers': []}, 411, 'content_length_required', 'get'),
            (('/v1/get', b'{}'), {'headers': [(b'content-length', b'2\n')]}, 411, 'content_length_required', 'get'),
            (('/v1/get', b'{}'), {'headers': [(b'content-length', b'2'), (b'Content-Length', b'2')]},
             411, 'content_length_required', 'get'),
            (('/v1/upload', b'{}'), {'headers': [(b'content-length', big)]}, 413, 'body_too_large', 'upload'),
            (('/v1/get', b'{}'), {'headers': [(b'content-length', small)]}, 413, 'body_too_large', 'get'),
            (('/v1/create', b'{}'), {'headers': [(b'content-length', small)]}, 413, 'body_too_large', 'create'),
            (('/v1/run-once', b'{}'), {'headers': [(b'content-length', run_big)]}, 413, 'body_too_large', 'run_once'),
            (('/v1/get', b'{}'), {'headers': [(b'content-length', b'5')]}, 400, 'body_length_mismatch', 'get'),
            (('/v1/get', b'{"name":"a"}'), {'headers': [(b'content-length', b'3')]}, 413, 'body_too_large', 'get'),
            (('/v1/get', b'{"name":'), {}, 400, 'invalid_json', 'get'),
            (('/v1/get', b'{"name":"a","name":"b"}'), {}, 400, 'invalid_json', 'get'),
            (('/v1/get', b'\xff'), {}, 400, 'invalid_json', 'get'),
            (('/v1/get', b'[]'), {}, 422, 'body_not_object', 'get'),
        ]
        for (path, body), kw, status, code, op in cases:
            with self.subTest(code=code, kw=kw):
                got_status, out = call_asgi(self.facade, path, body, **kw)
                self.assertEqual((got_status, out['refusal']), (status, code), out)
                self.assertEnvelope(out, op)

    def test_request_aborted(self):
        body = b'{"name":"lease-1"}'
        sent = []
        messages = [{'type': 'http.request', 'body': body[:4], 'more_body': True}, {'type': 'http.disconnect'}]

        async def receive():
            return messages.pop(0)

        async def send(message):
            sent.append(message)

        scope = {'type': 'http', 'path': '/v1/get', 'method': 'POST',
                 'headers': [(b'content-length', str(len(body)).encode())]}
        asyncio.run(self.facade.lease_asgi(scope, receive, send))
        out = json.loads(sent[1]['body'])
        self.assertEqual((sent[0]['status'], out['refusal']), (400, 'request_aborted'))

    def test_structure_heavy_body_is_refused_before_parsing(self):
        parsed = []
        real = self.facade.json
        self.facade.json = types.SimpleNamespace(
            loads=lambda *a, **k: parsed.append(1) or real.loads(*a, **k), dumps=real.dumps)
        body = json.dumps({'name': [{'a': 1}] * 5000}).encode()[: 64 * 1024 - 1]
        status, out = call_asgi(self.facade, '/v1/get', body)
        self.assertEqual((status, out['refusal']), (400, 'too_many_structures'))
        for deep in (b'[' * 40, b'{"a":' * 40):
            status, out = call_asgi(self.facade, '/v1/get', deep)
            self.assertEqual((status, out['refusal']), (400, 'too_many_structures'))
        self.assertEqual(parsed, [])
        self.facade.json = real

    def test_deep_nesting_is_invalid_json_even_without_the_structure_cap(self):
        self.facade.MAX_STRUCTURE_BYTES = 10 ** 9
        self.facade.ROUTE_BODY_BYTES['get'] = 10 ** 9
        for deep in (b'[' * 200_000, b'{"a":' * 200_000):
            status, out = call_asgi(self.facade, '/v1/get', deep)
            self.assertEqual((status, out['refusal']), (400, 'invalid_json'))

    def test_create_sized_request_fits_the_structure_cap(self):
        body = json.dumps(create_body(network=allow(*[f'h{i}.example.com' for i in range(4)]))).encode()
        status, out = call_asgi(self.facade, '/v1/create', body)
        self.assertEqual((status, out['ok']), (200, True))

    def test_invalid_json_is_not_echoed(self):
        status, out = call_asgi(self.facade, '/v1/get', b'{"token": "ak-secret"')
        self.assertEqual(status, 400)
        self.assertNotIn('ak-secret', json.dumps(out))

    # Create

    def test_create_applies_only_the_fixed_policy(self):
        out = self.created()
        self.assertEnvelope(out, 'create')
        self.assertTrue(out['created'])
        self.assertEqual(out['state'], 'running')
        argv, app, name, kwargs = self.provider.create_calls[0]
        self.assertEqual(argv, self.facade.LEASE_COMMAND)
        self.assertEqual(argv, ('/bin/sh', '-c',
                                'while [ ! -d /ryeos/run.go ]; do sleep 1; done; '
                                'exec /ryeos/guest/entrypoint < /ryeos/run.stdin'))
        self.assertEqual(app.name, 'ryeos-lease-sandboxes')
        self.assertEqual(name, 'lease-1')
        self.assertIs(kwargs['image'], self.facade.GUEST_IMAGE)
        self.assertEqual(kwargs['image'].steps, (('debian_slim', '3.12'), ('run_commands', ('mkdir -p /ryeos/guest',))))
        self.assertEqual(kwargs['timeout'], 3600)
        self.assertEqual((kwargs['cpu'], kwargs['memory']), (1.0, 1024))
        self.assertIs(kwargs['block_network'], False)
        self.assertEqual(kwargs['outbound_domain_allowlist'], ['controller.example.com'])
        self.assertEqual(set(kwargs), {'image', 'timeout', 'cpu', 'memory', 'block_network', 'outbound_domain_allowlist'})
        sandbox = self.provider.by_id[out['sandbox_id']]
        self.assertEqual(sandbox.tags, {
            'ryeos_facade': '1', 'ryeos_lease_set': 'node-a', 'ryeos_lease_name': 'lease-1',
            'ryeos_policy': out['policy_sha256']})
        self.assertEqual(out['policy_sha256'], self.facade.policy_digest(out['policy']))

    def test_create_block_network(self):
        self.created(network={'type': 'block-network'})
        kwargs = self.provider.create_calls[0][3]
        self.assertIs(kwargs['block_network'], True)
        self.assertNotIn('outbound_domain_allowlist', kwargs)

    def test_create_is_idempotent_for_the_same_policy(self):
        first = self.created(network=allow('a.example.com', 'b.example.com'))
        second = self.created(network=allow('b.example.com', 'a.example.com'))
        self.assertFalse(second['created'])
        self.assertTrue(second['name_taken'])
        self.assertEqual(second['sandbox_id'], first['sandbox_id'])
        self.assertEqual(second['policy_sha256'], first['policy_sha256'])
        self.assertEqual(second['name'], 'lease-1')
        self.assertEqual(len(self.provider.by_id), 1)

    def test_name_taken_with_other_policy_is_refused(self):
        first = self.created()
        for override in (
            {'timeout_seconds': 7200}, {'lease_set': 'node-b'},
            {'network': {'type': 'block-network'}}, {'network': allow('other.example.com')},
        ):
            with self.subTest(override=override):
                out = self.refused(self.facade.handle_create, create_body(**override), 'name_taken_policy_mismatch')
                self.assertEqual(out['stored_policy_sha256'], first['policy_sha256'])
                self.assertEqual(out['sandbox_id'], first['sandbox_id'])
                self.assertNotEqual(out['requested_policy_sha256'], first['policy_sha256'])

    def test_name_taken_with_other_name_tag_is_refused(self):
        self.provider.add_foreign(name='lease-1', tags={
            'ryeos_facade': '1', 'ryeos_lease_set': 'node-a', 'ryeos_lease_name': 'lease-2',
            'ryeos_policy': 'a' * 64})
        out = self.refused(self.facade.handle_create, create_body(), 'name_taken_tag_mismatch')
        self.assertEqual(out['name'], 'lease-1')

    def test_lease_ending_between_already_exists_and_lookup_is_a_provider_error(self):
        self.created()
        real = self.provider.from_name

        def vanished(app_name, name):
            self.provider.by_name[name].returncode = 0
            return real(app_name, name)

        self.facade.modal.Sandbox.from_name = vanished
        status, out = self.facade.handle_create(create_body())
        self.assertEqual((status, out['provider_error']), (502, 'NotFoundError'))

    def test_untagged_lease_is_reachable_by_name(self):
        # The function that created it died before tagging it.
        orphan = self.provider.add_foreign(name='lease-1')
        out = self.refused(self.facade.handle_create, create_body(), 'name_taken_untagged')
        self.assertEqual((out['sandbox_id'], out['name']), (orphan.object_id, 'lease-1'))
        self.assertIn('get it by name and retry', out['detail'])
        status, got = self.facade.handle_get({'name': 'lease-1'})
        self.assertEqual((status, got['found'], got['tagged']), (200, True, False))
        status, listed = self.facade.handle_list({'lease_set': 'node-a'})
        self.assertEqual(listed['untagged_sandbox_ids'], [orphan.object_id])
        status, term = self.facade.handle_terminate({'name': 'lease-1'})
        self.assertEqual((status, term['state'], term['tagged']), (200, 'terminated', False))
        self.assertTrue(self.created()['created'])

    def test_untaggable_lease_is_terminated_and_message_not_echoed(self):
        self.provider.fail_next_set_tags = True
        status, out = self.facade.handle_create(create_body())
        self.assertEqual(status, 502)
        self.assertEqual(out['provider_error'], 'ProviderBoom')
        self.assertNotIn('secret-looking', json.dumps(out))
        (sandbox,) = self.provider.by_id.values()
        self.assertIsNotNone(sandbox.returncode)

    def test_provider_error_reports_class_only(self):
        self.provider.create_error = ProviderBoom('token ak-secret leaked in message')
        status, body = self.facade.handle_create(create_body())
        self.assertEqual((status, body['ok'], body['provider_error']), (502, False, 'ProviderBoom'))
        self.assertNotIn('ak-secret', json.dumps(body))
        self.assertEnvelope(body, 'create')

    def test_policy_refusals_by_name(self):
        h = self.facade.handle_create

        def rule(**r):
            return {'type': 'domain-allow-list', 'rules': [dict({'protocol': 'https', 'host': 'a.example.com'}, **r)]}

        cases = [
            (create_body(network=rule(host='*.example.com')), 'wildcard_host_refused'),
            (create_body(network=rule(host='*')), 'wildcard_host_refused'),
            (create_body(network=rule(protocol='http')), 'plain_http_refused'),
            (create_body(network=rule(protocol='tcp')), 'non_https_rule_refused'),
            (create_body(network=rule(port=80)), 'port_refused'),
            (create_body(network={'type': 'domain-allow-list', 'rules': [{'protocol': 'https'}]}), 'missing_field'),
            (create_body(network={'type': 'domain-allow-list', 'rules': ['a.example.com']}), 'invalid_rule'),
            (create_body(network={'type': 'domain-allow-list', 'rules': []}), 'empty_allow_list'),
            (create_body(network=allow(*[f'h{i}.example.com' for i in range(5)])), 'too_many_rules'),
            (create_body(network=allow('a.example.com', 'a.example.com')), 'duplicate_host'),
            (create_body(network={'type': 'allow-all'}), 'invalid_network_type'),
            (create_body(network='block-network'), 'invalid_network'),
            (create_body(network={'type': 'block-network', 'rules': []}), 'unknown_field'),
            (create_body(timeout_seconds=86_401), 'timeout_out_of_range'),
            (create_body(timeout_seconds=59), 'timeout_out_of_range'),
            (create_body(timeout_seconds=True), 'timeout_out_of_range'),
            (create_body(timeout_seconds=3600.0), 'timeout_out_of_range'),
            (create_body(image='python:3.12'), 'image_refused'),
            (create_body(command=['sh']), 'command_refused'),
            (create_body(entrypoint='sh'), 'command_refused'),
            (create_body(args=['-c']), 'command_refused'),
            (create_body(gpu='T4'), 'gpu_refused'),
            (create_body(secrets=['x']), 'secrets_refused'),
            (create_body(volumes={}), 'volumes_refused'),
            (create_body(cidr_allowlist=['0.0.0.0/0']), 'cidr_refused'),
            (create_body(encrypted_ports=[443]), 'ports_refused'),
            (create_body(unencrypted_ports=[80]), 'ports_refused'),
            (create_body(cpu=8), 'resources_refused'),
            (create_body(memory=4096), 'resources_refused'),
            (create_body(region='us'), 'unknown_field'),
            ({k: v for k, v in create_body().items() if k != 'network'}, 'missing_field'),
            ([], 'body_not_object'),
        ]
        for body, code in cases:
            with self.subTest(code=code, body=body):
                self.refused(h, body, code)
        self.assertEqual(self.provider.create_calls, [])

    def test_host_syntax(self):
        accepted = ['a.example.com', 'xn--bcher-kva.example', 'a-b.c0', 'x.xn--p1ai', ('a' * 63) + '.com']
        refused = [
            '10.0.0.1', '2130706433', '::1', '[::1]', 'fe80::1', 'Example.com', 'localhost',
            'https://a.example.com', 'a.example.com.', 'a.example.com:443', 'a..example.com',
            '.example.com', '-a.example.com', 'a-.example.com', ('a' * 64) + '.com',
            '.'.join(['a' * 60] * 5), 'a.example.com\n', 'a.example.com ', 'a.example.1', '', 7,
        ]
        for host in accepted:
            with self.subTest(host=host):
                self.assertEqual(self.facade.parse_network(allow(host))['outbound_domain_allowlist'], [host])
        for host in refused:
            with self.subTest(host=host):
                self.refused(self.facade.handle_create, create_body(network=allow(host)), 'invalid_host')

    def test_name_syntax(self):
        for value in ('Lease_1', '', '-a', 'a' * 64, 'lease-1\n', 'lease 1', 3):
            with self.subTest(value=value):
                self.refused(self.facade.handle_create, create_body(name=value), 'invalid_name')
                self.refused(self.facade.handle_create, create_body(lease_set=value), 'invalid_lease_set')
                self.refused(self.facade.handle_list, {'lease_set': value}, 'invalid_lease_set')
        self.assertTrue(self.created(name='a' * 63)['created'])

    def test_sandbox_id_and_sha_syntax(self):
        for value in ('../sb', 'sb-', 'sb-abc\n', 'sb-a_b', 'SB-abc', 'sb-' + 'a' * 65):
            with self.subTest(value=value):
                self.refused(self.facade.handle_get, {'sandbox_id': value}, 'invalid_sandbox_id')
        good = hashlib.sha256(b'x').hexdigest()
        for value in (good.upper(), good + '\n', good[:-1], 5):
            with self.subTest(value=value):
                self.refused(self.facade.handle_upload,
                             {'sandbox_id': 'sb-x', 'content_b64': b64(b'x'), 'sha256': value}, 'invalid_sha256')

    def test_unknown_field_label_is_not_echoed_when_unprintable(self):
        for field in ('Bearer abc.def', 'region\n'):
            with self.subTest(field=field):
                out = self.refused(self.facade.handle_create, dict(create_body(), **{field: 1}), 'unknown_field')
                self.assertIn('<unprintable>', out['detail'])
        out = self.refused(self.facade.handle_create, dict(create_body(), region=1), 'unknown_field')
        self.assertIn("'region'", out['detail'])

    # Get and ownership

    def test_get_by_id_and_by_name(self):
        out = self.created()
        status, by_id = self.facade.handle_get({'sandbox_id': out['sandbox_id']})
        status2, by_name = self.facade.handle_get({'name': 'lease-1'})
        self.assertEqual((status, status2), (200, 200))
        for body in (by_id, by_name):
            self.assertEnvelope(body, 'get')
            self.assertEqual((body['found'], body['tagged'], body['name_verified']), (True, True, True))
            self.assertEqual(body['sandbox_id'], out['sandbox_id'])
            self.assertEqual(body['tags'], {'ryeos_lease_set': 'node-a', 'ryeos_lease_name': 'lease-1',
                                            'ryeos_policy': out['policy_sha256']})

    def test_get_after_termination_by_id(self):
        out = self.created()
        self.facade.handle_terminate({'sandbox_id': out['sandbox_id']})
        status, body = self.facade.handle_get({'sandbox_id': out['sandbox_id']})
        self.assertEqual((status, body['state'], body['name_verified']), (200, 'terminated', False))
        status, body = self.facade.handle_get({'name': 'lease-1'})
        self.assertEqual((status, body['found']), (200, False))

    def test_get_absent_and_selector_rules(self):
        for body in ({'name': 'absent'}, {'sandbox_id': 'sb-absent'}):
            status, out = self.facade.handle_get(body)
            self.assertEqual((status, out['found']), (200, False))
            self.assertEnvelope(out, 'get')
        self.refused(self.facade.handle_get, {}, 'ambiguous_selector')
        self.refused(self.facade.handle_get, {'name': 'a', 'sandbox_id': 'sb-x'}, 'ambiguous_selector')

    def test_absent_sandbox_outside_get_and_terminate_is_a_provider_error(self):
        for handler, body in (
            (self.facade.handle_upload, {'sandbox_id': 'sb-absent', 'content_b64': b64(b'x'),
                                         'sha256': hashlib.sha256(b'x').hexdigest()}),
            (self.facade.handle_run_once, {'sandbox_id': 'sb-absent', 'stdin_b64': ''}),
        ):
            status, out = handler(body)
            self.assertEqual((status, out['provider_error']), (502, 'NotFoundError'))
            self.assertNotIn('found', out)

    def test_foreign_sandboxes_are_refused(self):
        untagged = self.provider.add_foreign()
        # Carries copied tags but is not the lease app's sandbox of that name.
        lease = self.created()
        impostor = self.provider.add_foreign(tags=self.provider.by_id[lease['sandbox_id']].tags)
        data = b'x'
        for sandbox, code in ((untagged, 'not_facade_sandbox'), (impostor, 'not_lease_app_sandbox')):
            sid = sandbox.object_id
            for handler, body in (
                (self.facade.handle_get, {'sandbox_id': sid}),
                (self.facade.handle_terminate, {'sandbox_id': sid}),
                (self.facade.handle_upload, {'sandbox_id': sid, 'content_b64': b64(data),
                                             'sha256': hashlib.sha256(data).hexdigest()}),
                (self.facade.handle_run_once, {'sandbox_id': sid, 'stdin_b64': ''}),
            ):
                with self.subTest(code=code, handler=handler.__name__):
                    self.refused(handler, body, code)
            self.assertIsNone(sandbox.returncode)
            self.assertEqual((sandbox.execs, sandbox.files), ([], {}))

    def test_terminated_impostor_by_id_is_only_self_asserted(self):
        lease = self.created()
        impostor = self.provider.add_foreign(tags=self.provider.by_id[lease['sandbox_id']].tags)
        impostor.returncode = 0
        status, got = self.facade.handle_get({'sandbox_id': impostor.object_id})
        self.assertEqual((status, got['tagged'], got['name_verified']), (200, True, False))
        status, term = self.facade.handle_terminate({'sandbox_id': impostor.object_id})
        self.assertEqual((status, term['already_terminal']), (200, True))
        self.refused(self.facade.handle_run_once, {'sandbox_id': impostor.object_id, 'stdin_b64': ''},
                     'sandbox_terminated')
        self.assertIsNone(self.provider.by_id[lease['sandbox_id']].returncode)

    # Terminate

    def test_terminate_observes_terminal(self):
        out = self.created()
        status, body = self.facade.handle_terminate({'sandbox_id': out['sandbox_id']})
        self.assertEqual(status, 200)
        self.assertEnvelope(body, 'terminate')
        self.assertEqual((body['state'], body['returncode'], body['already_terminal']), ('terminated', 137, False))
        status, again = self.facade.handle_terminate({'sandbox_id': out['sandbox_id']})
        self.assertEqual((status, again['already_terminal']), (200, True))
        for selector in ({'sandbox_id': 'sb-nothere'}, {'name': 'lease-1'}):
            status, absent = self.facade.handle_terminate(selector)
            self.assertEqual((status, absent['found']), (200, False))

    def test_terminate_without_terminal_observation_is_502(self):
        out = self.created()
        sandbox = self.provider.by_id[out['sandbox_id']]
        sandbox.stuck = True
        clock = [1000.0]
        sleeps = []

        def sleep(seconds):
            sleeps.append(seconds)
            clock[0] += seconds * 2.5  # each poll is slow; the deadline is wall-clock

        self.facade._sleep = sleep
        self.facade._monotonic = lambda: clock[0]
        status, body = self.facade.handle_terminate({'name': 'lease-1'})
        self.assertEqual((status, body['provider_error'], body['state']), (502, 'terminal_state_not_observed', 'running'))
        self.assertEqual(len(sleeps), 24)

    # Upload

    def test_upload_once_to_fixed_path(self):
        out = self.created()
        data = b'\x7fELF guest'
        body = {'sandbox_id': out['sandbox_id'], 'content_b64': b64(data), 'sha256': hashlib.sha256(data).hexdigest()}
        status, up = self.facade.handle_upload(body)
        self.assertEqual(status, 200, up)
        self.assertEnvelope(up, 'upload')
        self.assertEqual((up['path'], up['bytes']), ('/ryeos/guest/entrypoint', len(data)))
        sandbox = self.provider.by_id[out['sandbox_id']]
        self.assertEqual(sandbox.files, {'/ryeos/guest/entrypoint': data})
        self.assertEqual(sandbox.execs, [
            ('mkdir', '/ryeos/upload.claimed'), ('chmod', '0755', '/ryeos/guest/entrypoint'),
            ('mkdir', '/ryeos/upload.done')])
        self.assertEqual([t.get('ryeos_upload') for t in sandbox.tag_history[1:]], ['claimed', body['sha256']])
        self.refused(self.facade.handle_upload, body, 'upload_already_claimed')
        # The guest removing its claim directory does not reopen the upload.
        sandbox.dirs.discard('/ryeos/upload.claimed')
        self.refused(self.facade.handle_upload, body, 'upload_already_claimed')

    def test_upload_claim_directory_alone_blocks(self):
        out = self.created()
        sandbox = self.provider.by_id[out['sandbox_id']]
        sandbox.dirs.add('/ryeos/upload.claimed')
        self.refused(self.facade.handle_upload, {'sandbox_id': out['sandbox_id'], 'content_b64': b64(b'x'),
                                                 'sha256': hashlib.sha256(b'x').hexdigest()}, 'upload_already_claimed')

    def test_upload_refusals(self):
        out = self.created()
        sid = out['sandbox_id']
        good = hashlib.sha256(b'x').hexdigest()
        self.refused(self.facade.handle_upload, {'sandbox_id': sid, 'content_b64': b64(b'y'), 'sha256': good}, 'sha256_mismatch')
        self.refused(self.facade.handle_upload, {'sandbox_id': sid, 'content_b64': '!!', 'sha256': good}, 'invalid_base64')
        self.refused(self.facade.handle_upload, {'sandbox_id': sid, 'content_b64': b64(b'x'), 'sha256': good, 'path': '/etc/x'}, 'path_refused')
        self.facade.MAX_UPLOAD_BYTES = 4
        self.refused(self.facade.handle_upload, {'sandbox_id': sid, 'content_b64': b64(b'12345'), 'sha256': good}, 'content_too_large')
        self.facade.MAX_UPLOAD_BYTES = 64 * 1024 * 1024
        self.facade.handle_terminate({'sandbox_id': sid})
        self.refused(self.facade.handle_upload, {'sandbox_id': sid, 'content_b64': b64(b'x'), 'sha256': good}, 'sandbox_terminated')
        self.assertEqual(self.provider.by_id[sid].files, {})

    def test_oversized_base64_is_refused_before_decoding(self):
        decoded = []
        real = self.facade.base64
        self.facade.base64 = types.SimpleNamespace(b64decode=lambda *a, **k: decoded.append(1) or real.b64decode(*a, **k))
        limit = self.facade.MAX_STDIN_BYTES
        self.refused(self.facade.handle_run_once,
                     {'sandbox_id': 'sb-x', 'stdin_b64': 'A' * ((limit // 3 + 1) * 4 + 4)}, 'stdin_too_large')
        self.assertEqual(decoded, [])

    def test_failed_fixed_step_is_refused(self):
        out = self.created()
        sandbox = self.provider.by_id[out['sandbox_id']]
        sandbox.fail_exec.add('chmod')
        self.refused(self.facade.handle_upload, {'sandbox_id': out['sandbox_id'], 'content_b64': b64(b'x'),
                                                 'sha256': hashlib.sha256(b'x').hexdigest()}, 'guest_step_failed')
        self.refused(self.facade.handle_run_once, {'sandbox_id': out['sandbox_id'], 'stdin_b64': ''}, 'upload_not_complete')

    # Run once

    def test_run_once_releases_the_guest_with_documented_stdin(self):
        out, sandbox = self.uploaded()
        stdin = b'{"bootstrap":1}\n'
        status, run = self.facade.handle_run_once({'sandbox_id': out['sandbox_id'], 'stdin_b64': b64(stdin)})
        self.assertEqual(status, 200, run)
        self.assertEnvelope(run, 'run_once')
        self.assertEqual((run['released'], run['argv'], run['stdin_path']),
                         (True, ['/ryeos/guest/entrypoint'], '/ryeos/run.stdin'))
        self.assertEqual((run['stdin_bytes'], run['stdin_sha256']), (len(stdin), hashlib.sha256(stdin).hexdigest()))
        self.assertEqual(sandbox.files['/ryeos/run.stdin'], stdin)
        self.assertEqual(sandbox.execs[-3:], [
            ('test', '-d', '/ryeos/upload.done'), ('mkdir', '/ryeos/run.claimed'), ('mkdir', '/ryeos/run.go')])
        self.assertEqual(sandbox.tags['ryeos_run'], run['stdin_sha256'])
        self.refused(self.facade.handle_run_once, {'sandbox_id': out['sandbox_id'], 'stdin_b64': ''}, 'run_once_already_claimed')
        sandbox.dirs.discard('/ryeos/run.claimed')
        self.refused(self.facade.handle_run_once, {'sandbox_id': out['sandbox_id'], 'stdin_b64': ''}, 'run_once_already_claimed')
        # The guest is the sandbox command, so its exit is the lease's terminal state.
        sandbox.returncode = 3
        status, got = self.facade.handle_get({'sandbox_id': out['sandbox_id']})
        self.assertEqual((got['state'], got['returncode']), ('terminated', 3))

    def test_run_once_requires_completed_upload(self):
        out = self.created()
        sid = out['sandbox_id']
        sandbox = self.provider.by_id[sid]
        self.refused(self.facade.handle_run_once, {'sandbox_id': sid, 'stdin_b64': ''}, 'upload_not_complete')
        sandbox.dirs.add('/ryeos/upload.done')  # guest-made marker without the tag
        self.refused(self.facade.handle_run_once, {'sandbox_id': sid, 'stdin_b64': ''}, 'upload_not_complete')
        self.assertNotIn('/ryeos/run.go', sandbox.dirs)

    def test_run_once_refusals(self):
        out, _ = self.uploaded()
        sid = out['sandbox_id']
        self.refused(self.facade.handle_run_once, {'sandbox_id': sid, 'stdin_b64': '', 'command': 'sh'}, 'command_refused')
        self.refused(self.facade.handle_run_once, {'sandbox_id': sid, 'stdin_b64': '', 'args': ['-c']}, 'command_refused')
        self.refused(self.facade.handle_run_once, {'sandbox_id': sid, 'stdin_b64': 'not base64!'}, 'invalid_base64')
        self.refused(self.facade.handle_run_once, {'sandbox_id': sid, 'stdin_b64': b64(b'x' * (64 * 1024 + 1))}, 'stdin_too_large')

    # List

    def test_list_by_lease_set(self):
        a = self.created(name='lease-a')
        self.created(name='lease-b', lease_set='node-b')
        status, body = self.facade.handle_list({'lease_set': 'node-a'})
        self.assertEqual(status, 200)
        self.assertEnvelope(body, 'list')
        self.assertEqual((body['sandbox_ids'], body['untagged_sandbox_ids'], body['truncated']),
                         ([a['sandbox_id']], [], False))
        self.assertEqual(self.provider.list_calls[-1], {'app_id': 'ap-leases'})
        self.refused(self.facade.handle_list, {'lease_set': 'node-a', 'tags': {}}, 'unknown_field')

    def test_list_truncates(self):
        self.facade.MAX_LIST_RESULTS = 2
        ids = [self.created(name=f'lease-{i}')['sandbox_id'] for i in range(3)]
        status, body = self.facade.handle_list({'lease_set': 'node-a'})
        self.assertEqual((body['sandbox_ids'], body['truncated']), (ids[:2], True))
        self.facade.MAX_LIST_RESULTS = 3
        status, body = self.facade.handle_list({'lease_set': 'node-a'})
        self.assertEqual((body['sandbox_ids'], body['truncated']), (ids, False))


if __name__ == '__main__':
    unittest.main()
