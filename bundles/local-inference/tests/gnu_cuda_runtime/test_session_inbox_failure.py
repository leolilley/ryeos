"""Actual GNU inbox source controls; no model/native/provider qualification."""
import contextlib
import importlib.util
import os
from pathlib import Path
import queue
import socket
import sys
import threading
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
SOURCE = Path(__file__).resolve().parents[2] / '.ai/workers/local-inference/lib/local-tinygrad-gnu'


class InboxFailureTests(unittest.TestCase):
    def setUp(self):
        spec = importlib.util.spec_from_file_location('gnu_inbox_failure_fixture', SOURCE / 'session.py')
        self.session = importlib.util.module_from_spec(spec)
        # Load the complete actual GNU module, not AST-extracted methods or CPU
        # copies. Neither model nor tokenizer construction is part of this test.
        with patch.dict(sys.modules, {
            'model': SimpleNamespace(QwenModel=object),
            'tokenizer': SimpleNamespace(QwenTokenizer=object, render_chat=lambda *args: ''),
        }):
            spec.loader.exec_module(self.session)

    def capture_failure(self, inbox, primary):
        try:
            inbox.next_request()
        except BaseException as error:
            self.assertIs(error, primary)
            return error
        self.fail('fatal inbox dispatched a request')

    def test_full_queue_reader_failure_returns_without_discarding_pending_request(self):
        inbox = self.session.RequestInbox(-1)
        pending = ('one', {}, threading.Event())
        inbox.requests.put_nowait(pending)
        primary = ValueError('malformed frame after queued request')
        with patch.object(self.session, '_read_frame', side_effect=primary):
            reader = threading.Thread(target=inbox.run, daemon=True)
            reader.start()
            try:
                reader.join(1)
                self.assertFalse(reader.is_alive(), 'failure reporting blocked on full queue')
                self.capture_failure(inbox, primary)
                self.assertIs(inbox.requests.get_nowait(), pending)
            finally:
                # Also release a regressed blocking-put implementation so the
                # fixture does not abandon its thread after an assertion fails.
                try:
                    inbox.requests.get_nowait()
                except queue.Empty:
                    pass
                reader.join(1)

    def test_empty_queue_failure_wakes_already_waiting_consumer(self):
        inbox = self.session.RequestInbox(-1)
        waiting = threading.Event()
        caught = []
        original_get = inbox.requests.get
        def get():
            waiting.set()
            return original_get()
        def consume():
            try:
                inbox.next_request()
            except BaseException as error:
                caught.append(error)
        primary = EOFError('truncated input')
        with patch.object(inbox.requests, 'get', side_effect=get):
            consumer = threading.Thread(target=consume, daemon=True)
            consumer.start()
            try:
                self.assertTrue(waiting.wait(1), 'consumer never reached queue wait')
                with patch.object(self.session, '_read_frame', side_effect=primary):
                    inbox.run()
                consumer.join(1)
                self.assertFalse(consumer.is_alive())
                self.assertEqual(len(caught), 1)
                self.assertIs(caught[0], primary)
            finally:
                if consumer.is_alive():
                    inbox.requests.put_nowait(primary)
                    consumer.join(1)

    def test_repeated_recording_and_observation_preserve_same_first_exception(self):
        inbox = self.session.RequestInbox(-1)
        first = self.session.SessionInputClosed('first clean EOF')
        later = EOFError('later truncation must not replace first')
        inbox._record_failure(first)
        inbox._record_failure(later)  # full wake slot must not block
        self.capture_failure(inbox, first)
        self.capture_failure(inbox, first)
        self.assertIs(inbox.requests.get_nowait(), first)

    def test_failure_after_queue_get_refuses_dispatch_and_keeps_current_owner(self):
        inbox = self.session.RequestInbox(-1)
        pending = ('one', {}, threading.Event())
        inbox._current = (pending[0], pending[2])
        inbox.requests.put_nowait(pending)
        original_get = inbox.requests.get
        primary = ValueError('fatal input raced with dequeue')
        def get():
            item = original_get()
            inbox._record_failure(primary)
            return item
        dispatched = []
        with patch.object(inbox.requests, 'get', side_effect=get):
            try:
                dispatched.append(inbox.next_request())
            except BaseException as error:
                self.assertIs(error, primary)
            else:
                self.fail('post-dequeue fatal error was not observed')
        self.assertFalse(dispatched)
        self.assertEqual(inbox._current[0], pending[0])
        self.assertIs(inbox._current[1], pending[2])
        self.assertFalse(pending[2].is_set(), 'failure reporting fabricated cancellation')

    def test_failure_reporting_does_not_wait_for_terminal_publication_lock(self):
        inbox = self.session.RequestInbox(-1)
        cancelled = threading.Event()
        inbox._current = ('one', cancelled)
        entered = threading.Event()
        release = threading.Event()
        published = []
        def publish():
            entered.set()
            if not release.wait(2):
                raise AssertionError('fixture publication was not released')
            published.append(True)
        publisher = threading.Thread(target=lambda: inbox.complete('one', publish), daemon=True)
        publisher.start()
        primary = ValueError('reader failed while publication held current lock')
        reader = threading.Thread(target=inbox.run, daemon=True)
        try:
            self.assertTrue(entered.wait(1))
            with patch.object(self.session, '_read_frame', side_effect=primary):
                reader.start()
                reader.join(1)
                self.assertFalse(reader.is_alive(), 'failure custody acquired publication lock')
                self.capture_failure(inbox, primary)
            self.assertFalse(published)
        finally:
            release.set()
            publisher.join(1)
            if reader.ident is not None:
                reader.join(1)
        self.assertFalse(publisher.is_alive())
        self.assertEqual(published, [True])
        self.assertIsNone(inbox._current)

    def test_actual_framing_preserves_clean_eof_and_truncation_exception_identity(self):
        for data, clean in ((b'', True), (b'\x00\x00', False), (b'\x00\x00\x00\x04{', False)):
            with self.subTest(data=data):
                peer, worker = socket.socketpair()
                try:
                    if data:
                        peer.sendall(data)
                    peer.close()
                    inbox = self.session.RequestInbox(worker.fileno())
                    inbox.run()
                    first = inbox.requests.get_nowait()
                    self.assertIsInstance(first, EOFError)
                    self.assertEqual(isinstance(first, self.session.SessionInputClosed), clean)
                    self.capture_failure(inbox, first)
                    self.capture_failure(inbox, first)
                finally:
                    peer.close()
                    worker.close()

    def test_actual_main_uses_fatal_check_before_worker_dispatch(self):
        peer, worker = socket.socketpair()
        self.addCleanup(peer.close)
        self.addCleanup(worker.close)
        inbox = self.session.RequestInbox(worker.fileno())
        pending = ('one', {}, threading.Event())
        inbox.requests.put_nowait(pending)
        primary = ValueError('fatal input retained before main consumes request')
        inbox._record_failure(primary)
        model = SimpleNamespace(execute=lambda *args: self.fail('queued request executed'))
        lifecycle = SimpleNamespace(submission=contextlib.nullcontext)
        # The actual loop is used; only setup/model and already-terminal reader
        # execution are isolated. Production still owns its actual thread here.
        with patch.object(self.session, 'Worker', return_value=model), \
                patch.object(self.session, 'RequestInbox', return_value=inbox), \
                patch.object(inbox, 'run'), \
                patch.object(self.session, '_write_frame'), \
                patch.dict(os.environ, {'RYEOS_SESSION_FD': str(worker.fileno())}):
            try:
                self.session.main(lifecycle)
            except BaseException as error:
                self.assertIs(error, primary)
            else:
                self.fail('main did not refuse retained fatal input')
        self.assertIs(inbox.requests.get_nowait(), pending)


if __name__ == '__main__':
    unittest.main()
