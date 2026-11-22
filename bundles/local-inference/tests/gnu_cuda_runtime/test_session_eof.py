"""Real input framing and main loop; valid retirement cannot hide truncation."""
import contextlib
import importlib.util
import os
from pathlib import Path
import socket
import struct
import sys
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.dont_write_bytecode=True
SOURCE=Path(__file__).resolve().parents[2]/'.ai/workers/local-inference/lib/local-tinygrad-gnu'
sys.path.insert(0,str(SOURCE))
import cleanup_channel
import runtime
from test_cleanup_channel import frame, BINDINGS

class SessionEOFTests(unittest.TestCase):
    def run_input(self, data, clean):
        spec=importlib.util.spec_from_file_location('framing_session_fixture',SOURCE/'session.py')
        session=importlib.util.module_from_spec(spec)
        with patch.dict(sys.modules,{'model':SimpleNamespace(QwenModel=object),
                 'tokenizer':SimpleNamespace(QwenTokenizer=object,render_chat=lambda *args:'')}):
            spec.loader.exec_module(session)
        peer,worker=socket.socketpair()
        self.addCleanup(worker.close)
        if data:os.write(peer.fileno(),data)
        peer.close()  # actual request input EOF after the selected byte prefix
        parent,child=socket.socketpair()
        self.addCleanup(parent.close)
        fd=child.detach()
        self.addCleanup(lambda: self.close(fd))
        os.write(parent.fileno(),frame('init')+frame('shutdown'))
        channel=cleanup_channel.CleanupChannel(fd,worker.fileno())
        retired=[]
        def shutdown():
            channel.observe(SimpleNamespace(selected_device=0))
            retired.append(True)
            return SimpleNamespace(status='closed',observation_delivered=True)
        lifecycle=SimpleNamespace(submission=contextlib.nullcontext,shutdown=shutdown)
        with patch.object(session,'Worker',return_value=object()), \
             patch.object(session,'_write_frame'), \
             patch.dict(os.environ,{'RYEOS_SESSION_FD':str(worker.fileno())}):
            if clean:
                self.assertEqual(runtime.run_with_lifecycle(lifecycle,lambda:session,channel.authorize_shutdown),0)
            else:
                with self.assertRaisesRegex(EOFError,'truncated') as error:
                    runtime.run_with_lifecycle(lifecycle,lambda:session,channel.authorize_shutdown)
                self.assertNotIsInstance(error.exception,session.SessionInputClosed)
        self.assertEqual(retired,[True])
        self.assertEqual(cleanup_channel.read_frame(parent.fileno(),'retired',BINDINGS),BINDINGS)
        self.assertEqual(os.read(parent.fileno(),1),b'')

    @staticmethod
    def close(fd):
        try:os.close(fd)
        except OSError:pass

    def test_clean_frame_boundary_can_finish_after_authorized_retirement(self):
        self.run_input(b'',True)

    def test_partial_header_remains_protocol_failure_after_authorized_retirement(self):
        header=struct.pack('>I',100)
        for length in (1,2,3):
            with self.subTest(length=length):self.run_input(header[:length],False)

    def test_partial_payload_remains_protocol_failure_after_authorized_retirement(self):
        for payload in (b'',b'{',b'{"kind":'):
            with self.subTest(payload=payload):self.run_input(struct.pack('>I',100)+payload,False)

if __name__=='__main__':unittest.main()
