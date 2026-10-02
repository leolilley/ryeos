"""Private wire fixtures; no driver or provider contact."""
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
def frame(kind, **changes):
    data=json.dumps(dict(BINDINGS,version=1,kind=kind,**changes),separators=(',',':')).encode()
    return struct.pack('>I',len(data))+data

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

    def test_init_shutdown_retired_eof_same_bindings_once(self):
        parent,fd=self.pair();os.write(parent.fileno(),frame('init')+frame('shutdown'))
        channel=wire.CleanupChannel(fd,0)
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
        valid=json.dumps(dict(BINDINGS,version=1,kind='init')).encode()
        for raw in (valid[:-1]+b',"unknown":0}', valid[:-1]+b',"version":1}',
                    valid.replace(b'"version": 1',b'"version": true'),
                    valid.replace(b'"version": 1',b'"version": NaN'),b'[]'):
            with self.assertRaises(ValueError): wire.decode(raw,'init')

    def test_oversized_length_refuses_before_body_read(self):
        parent,fd=self.pair();os.write(parent.fileno(),struct.pack('>I',4097))
        with self.assertRaises(ValueError): wire.read_frame(fd,'init')

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
