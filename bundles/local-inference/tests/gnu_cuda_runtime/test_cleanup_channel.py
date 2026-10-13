"""Private wire fixtures; no driver or provider contact."""
import copy
import json
import os
from pathlib import Path
import socket
import struct
import sys
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.dont_write_bytecode=True
sys.path.insert(0,str(Path(__file__).resolve().parents[2]/'.ai/workers/local-inference/lib/local-tinygrad-gnu'))
import cleanup_channel as wire
import runtime

BINDINGS={key:format(i,'064x') for i,key in enumerate(sorted(wire.BINDINGS),1)}
BOUNDS=dict(maximum_entries=100,maximum_depth=10,maximum_file_bytes=1024,maximum_total_bytes=4096)
def frame(kind, **changes):
    message=dict(BINDINGS,version=2,kind='init',uuid_v2_hex='00112233445566778899aabbccddeeff',
                 gnu_bounds=BOUNDS,driver_bounds=BOUNDS) if kind=='init' else dict(BINDINGS,version=1,kind=kind)
    message.update(changes)
    data=json.dumps(message,separators=(',',':')).encode()
    return struct.pack('>I',len(data))+data


# Candidate wire fixtures only; no independent device/provider/owner admission.
DEVICE = dict(role='primary', destination='/dev/nvidia0', access='read_write', major=195, minor=0)
PREFLIGHT_UUID = '1' * 32

def preflight_request():
    return dict(version=3, kind='preflight', bindings=dict(BINDINGS), challenge='8' * 64,
        inputs=dict(resource_selection=dict(stable_id='gpu0', **{'class': 'gpu'},
            matched_facts={}, observation_contract_digest='6'*64, device_binding_digest='7'*64,
            access='execution_restricted', enforcement='character_device_grant', character_devices=[dict(DEVICE)]),
            policy=dict(schema='ryeos.local_tinygrad_nvml_assignment_policy.v1', primary_device_role='primary', l4_query_policy=None),
            handler_source_sha256='9'*64, provider_manifest_sha256='a'*64,
            gnu_bounds=dict(BOUNDS), driver_bounds=dict(BOUNDS)))

def preflight_observation():
    return dict(version=1, stable_resource_id='gpu0', device_binding_digest='7'*64,
                primary_device=dict(DEVICE), uuid_v2_hex=PREFLIGHT_UUID, nvml_shutdown='closed')

def preflight_result():
    return dict(version=3, kind='preflight-result', bindings=dict(BINDINGS), challenge='8'*64,
                observation=preflight_observation())

def preflight_init():
    return dict(BINDINGS, version=2, kind='init', uuid_v2_hex=PREFLIGHT_UUID,
                gnu_bounds=dict(BOUNDS), driver_bounds=dict(BOUNDS))

def object_body(value):
    return json.dumps(value, separators=(',', ':'), allow_nan=False).encode()

def object_frame(value):
    data = object_body(value)
    return struct.pack('>I', len(data)) + data

def write_all(fd, data):
    offset = 0
    while offset < len(data):
        count = os.write(fd, data[offset:])
        if count <= 0: raise EOFError('control write failed')
        offset += count

class ChannelTests(unittest.TestCase):
    def pair(self):
        parent,child=socket.socketpair()
        self.addCleanup(parent.close)
        fd=child.detach()
        self.addCleanup(lambda: self.close(fd))
        return parent,fd
    @staticmethod
    def close(fd):
        try: os.close(fd)
        except OSError: pass

    def test_nested_request_closed_and_required_nullable_policy(self):
        original = preflight_request()
        for path in [(), ('bindings',), ('inputs',), ('inputs','policy'),
                     ('inputs','resource_selection'), ('inputs','resource_selection','character_devices',0),
                     ('inputs','gnu_bounds')]:
            value = copy.deepcopy(original); target = value
            for key in path: target = target[key]
            target['unknown'] = 1
            with self.subTest(path=path), self.assertRaises(ValueError): wire.decode_preflight(object_body(value))
        value = preflight_request(); del value['inputs']['policy']['l4_query_policy']
        with self.assertRaises(ValueError): wire.decode_preflight(object_body(value))
        raw = object_body(original).replace(b'"major":195', b'"major":195,"major":195')
        with self.assertRaises(ValueError): wire.decode_preflight(raw)

    def test_selection_policy_primary_integer_and_uuid_guards(self):
        paths_values = [(('version',), True), (('challenge',), '0'*64),
            (('inputs','policy','l4_query_policy'), 'automatic'),
            (('inputs','resource_selection','matched_facts'), {'uuid': None}),
            (('inputs','resource_selection','character_devices',0,'major'), True),
            (('inputs','resource_selection','character_devices',0,'minor'), 0x100000000),
            (('inputs','resource_selection','character_devices',0,'destination'), '/dev/../nvidia0'),
            (('inputs','gnu_bounds','maximum_entries'), 0x100000000)]
        for path, changed in paths_values:
            value = preflight_request(); target = value
            for key in path[:-1]: target = target[key]
            target[path[-1]] = changed
            with self.subTest(path=path), self.assertRaises(ValueError): wire.decode_preflight(object_body(value))
        value = preflight_request(); value['inputs']['resource_selection']['character_devices'].append(
            dict(DEVICE, role='secondary', destination='/dev/nvidia1', minor=1))
        with self.assertRaises(ValueError): wire.decode_preflight(object_body(value))
        for policy in wire.L4_POLICIES:
            value = preflight_request(); value['inputs']['policy']['l4_query_policy'] = policy
            wire.decode_preflight(object_body(value))  # shape only, not policy admission

    def test_snapshot_cannot_be_mutated_into_another_owner_and_result_is_not_proof(self):
        selected = wire.decode_preflight(object_body(preflight_request()))
        values = selected.values; values['bindings']['process_owner'] = 'f'*64
        self.assertEqual(selected.values['bindings'], BINDINGS)
        for path in [(), ('bindings',), ('observation',), ('observation','primary_device')]:
            value = preflight_result(); target = value
            for key in path: target = target[key]
            target['extra'] = 1
            with self.assertRaises(ValueError): wire.decode_preflight_result(object_body(value), selected)
        for key, changed in [('uuid_v2_hex','0'*32), ('nvml_shutdown','open'), ('version',True)]:
            value = preflight_result(); value['observation'][key] = changed
            with self.assertRaises(ValueError): wire.decode_preflight_result(object_body(value), selected)
        for key, changed in [('challenge','9'*64), ('kind','init'), ('version',2)]:
            value = preflight_result(); value[key] = changed
            with self.assertRaises(ValueError): wire.decode_preflight_result(object_body(value), selected)

    def test_same_live_socket_preflight_result_init_shutdown_retirement_eof(self):
        parent, fd = self.pair(); channel = wire.CleanupChannel.preflight(fd, 0)
        write_all(parent.fileno(),object_frame(preflight_request())); channel.read_preflight(); channel.report_preflight(preflight_observation())
        wire.decode_preflight_result(wire._read_body(parent.fileno()), wire.decode_preflight(object_body(preflight_request())))
        os.fstat(fd)  # reporting did not close the original stream
        write_all(parent.fileno(),object_frame(preflight_init()) + object_frame(dict(BINDINGS,version=1,kind='shutdown')))
        channel.accept_init(); self.assertEqual(channel.uuid_v2_hex, PREFLIGHT_UUID)
        channel.authorize_shutdown(); channel.observe(SimpleNamespace(selected_device=0))
        wire.read_frame(parent.fileno(),'retired',BINDINGS)
        self.assertEqual(os.read(parent.fileno(),1), b'')

    def test_duplicate_result_in_later_init_phase_fails_stickily_without_closing_owner(self):
        parent, fd = self.pair(); channel = wire.CleanupChannel.preflight(fd, 0)
        write_all(parent.fileno(),object_frame(preflight_request())); channel.read_preflight(); channel.report_preflight(preflight_observation())
        wire._read_body(parent.fileno()); write_all(parent.fileno(),object_frame(preflight_result()) + object_frame(preflight_init()))
        with self.assertRaises(ValueError): channel.accept_init()
        self.assertEqual(channel._phase,'failed'); os.fstat(fd)
        with self.assertRaises(RuntimeError): channel.accept_init()
        with self.assertRaises(RuntimeError): channel.authorize_shutdown()

    def test_premature_calls_wrong_init_uuid_and_partial_eof_keep_failure_custody(self):
        for operation in ('accept_init', 'authorize_shutdown'):
            parent, fd = self.pair(); channel = wire.CleanupChannel.preflight(fd, 0)
            with self.assertRaises(RuntimeError): getattr(channel, operation)()
            os.fstat(fd); self.assertEqual(channel._phase,'failed')
        parent, fd = self.pair(); channel = wire.CleanupChannel.preflight(fd, 0)
        write_all(parent.fileno(),object_frame(preflight_request())); channel.read_preflight(); channel.report_preflight(preflight_observation())
        wire._read_body(parent.fileno()); write_all(parent.fileno(),object_frame(dict(preflight_init(),uuid_v2_hex='2'*32)))
        with self.assertRaises(ValueError): channel.accept_init()
        os.fstat(fd); self.assertEqual(channel._phase,'failed')
        for raw in [struct.pack('>I',4097), struct.pack('>I',0), object_frame(preflight_request())[:-1]]:
            parent, fd = self.pair(); channel = wire.CleanupChannel.preflight(fd, 0)
            write_all(parent.fileno(),raw); parent.close()
            with self.assertRaises((EOFError,ValueError)): channel.read_preflight()
            os.fstat(fd); self.assertEqual(channel._phase,'failed')

    def test_init_shutdown_retired_eof_same_bindings_once(self):
        parent,fd=self.pair();os.write(parent.fileno(),frame('init')+frame('shutdown'))
        channel=wire.CleanupChannel(fd,0)
        self.assertEqual(dict(channel.bindings),BINDINGS)
        self.assertEqual(channel.uuid_v2_hex,'00112233445566778899aabbccddeeff')
        self.assertEqual(channel.gnu_bounds,wire.TreeBounds.from_contract(BOUNDS))
        channel.authorize_shutdown();channel.observe(SimpleNamespace(selected_device=0))
        bindings=wire.read_frame(parent.fileno(),'retired',BINDINGS)
        self.assertEqual(bindings,BINDINGS)
        self.assertEqual(os.read(parent.fileno(),1),b'')
        with self.assertRaises(RuntimeError): channel.observe(SimpleNamespace(selected_device=0))

    def test_no_success_on_early_eof_or_foreign_shutdown(self):
        for foreign in (False,True):
            parent,fd=self.pair();os.write(parent.fileno(),frame('init'))
            channel=wire.CleanupChannel(fd,0)
            if foreign:
                # Exactly bound init cannot be renewed by a different owner.
                data=dict(BINDINGS,process_owner='f'*64,version=1,kind='shutdown')
                raw=json.dumps(data).encode();os.write(parent.fileno(),struct.pack('>I',len(raw))+raw)
            else: parent.close()
            with self.assertRaises((EOFError,ValueError)): channel.authorize_shutdown()
            with self.assertRaises(RuntimeError): channel.observe(SimpleNamespace(selected_device=0))

    def test_strict_grammar_refuses_unknown_duplicate_nonfinite_and_wrong_version(self):
        valid=frame('init')[4:]
        for raw in (valid[:-1]+b',"unknown":0}', valid.replace(b'"version":2',b'"version":1'),
                    valid.replace(b'"version":2',b'"version":true'),
                    valid.replace(b'"version":2',b'"version":NaN'),b'[]'):
            with self.assertRaises(ValueError): wire.decode_init(raw)

    def test_oversized_length_refuses_before_body_read(self):
        parent,fd=self.pair();os.write(parent.fileno(),struct.pack('>I',4097))
        with self.assertRaises(ValueError): wire.read_init_frame(fd)

    def test_fd_collisions_stdio_and_noncanonical_refuse(self):
        for value in (None,'0','1','2','04','-1','2147483648'):
            with self.assertRaises(ValueError): wire.descriptor(value)
        parent,fd=self.pair()
        with self.assertRaises(ValueError): wire.CleanupChannel(fd,fd)

    def test_missing_cleanup_fd_refuses_before_gnu_prepare(self):
        with patch.dict(os.environ,{'RYEOS_SESSION_FD':'0'},clear=True), patch.object(runtime,'prepare_environment',side_effect=AssertionError('CUDA preparation')):
            with self.assertRaises(ValueError): runtime.run_channel_product()

    def test_invalid_shutdown_still_runs_local_retirement(self):
        calls=[]
        import contextlib
        life=SimpleNamespace(submission=contextlib.nullcontext,shutdown=lambda: calls.append('shutdown') or SimpleNamespace(status='failed',observation_delivered=False))
        def invalid():
            calls.append('authorization')
            raise ValueError('not authorized')
        with self.assertRaises(ValueError):
            runtime.run_with_lifecycle(life,lambda:SimpleNamespace(main=lambda _:0),invalid)
        self.assertEqual(calls,['authorization','shutdown'])

if __name__=='__main__':unittest.main()
