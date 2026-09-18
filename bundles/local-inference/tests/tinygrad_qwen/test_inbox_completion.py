"""I01: deterministic real-channel tests without admitted model fixtures."""
import ast
import json
import os
from pathlib import Path
import queue
import socket
import struct
import stat
import threading
import types
import unittest


def protocol():
    source = Path(__file__).parents[2] / ".ai/workers/local-inference/lib/local-tinygrad/session.py"
    tree = ast.parse(source.read_text())
    names = {"_read_exact", "_strict_object", "_reject_json_constant", "_strict_json_loads",
             "_read_frame", "_write_frame", "RequestInbox", "main"}
    tree.body = [node for node in tree.body if isinstance(node, (ast.FunctionDef, ast.ClassDef))
                 and node.name in names]
    namespace = dict(os=os, json=json, struct=struct, stat=stat, queue=queue, threading=threading,
                     Any=object, PROTOCOL="ryeos.persistent-session", VERSION=1,
                     MAX_FRAME_BYTES=16 * 1024 * 1024)
    exec(compile(tree, str(source), "exec"), namespace)
    return namespace


class InboxCompletionTests(unittest.TestCase):
    def test_i01_actual_main_loop_reuses_after_success_and_error(self):
        p = protocol()
        worker, peer = socket.socketpair()
        class FakeWorker:
            def __init__(self, **kwargs):
                pass
            def execute(self, request_id, body, cancelled, emit):
                if body.get("fail"):
                    raise ValueError("expected request error")
                return {"echo": request_id}
        p["Worker"] = FakeWorker
        p["os"] = types.SimpleNamespace(environ={"RYEOS_SESSION_FD": str(worker.fileno())},
                                        read=os.read, write=os.write, fstat=os.fstat)
        errors = []
        def run():
            try:
                p["main"]()
            except BaseException as error:
                errors.append(error)
        runner = threading.Thread(target=run, daemon=True)
        runner.start()
        try:
            self.assertEqual(p["_read_frame"](peer.fileno())["kind"], "ready")
            for index in range(20):
                request_id = str(index)
                p["_write_frame"](peer.fileno(), threading.Lock(), "request", request_id,
                                  {"fail": index % 2 == 1})
                response = p["_read_frame"](peer.fileno())
                self.assertEqual(response["request_id"], request_id)
                self.assertEqual(response["kind"], "error" if index % 2 else "final")
        finally:
            peer.close()
            runner.join(2)
            worker.close()
        self.assertFalse(runner.is_alive())
        self.assertEqual(len(errors), 1)
        self.assertIsInstance(errors[0], EOFError)

    def test_i01_immediate_reuse_after_final_and_error(self):
        for terminal in ("final", "error"):
            with self.subTest(terminal=terminal):
                p = protocol()
                worker, peer = socket.socketpair()
                inbox = p["RequestInbox"](worker.fileno())
                reader = threading.Thread(target=inbox.run, daemon=True)
                reader.start()
                lock = threading.Lock()
                published = threading.Event()
                release = threading.Event()
                next_request_checked = threading.Event()
                class ObservedLock:
                    def __init__(self):
                        self.lock = threading.Lock()
                    def __enter__(self):
                        if published.is_set():
                            next_request_checked.set()
                        self.lock.acquire()
                    def __exit__(self, *args):
                        self.lock.release()
                inbox._current_lock = ObservedLock()
                def write(kind, request_id):
                    p["_write_frame"](peer.fileno(), lock, kind, request_id, {})
                try:
                    write("request", "one")
                    self.assertEqual(inbox.requests.get(timeout=2)[0], "one")
                    def publish():
                        p["_write_frame"](worker.fileno(), lock, terminal, "one", {})
                        published.set()
                        if not release.wait(2):
                            raise TimeoutError("test did not release terminal publication")
                    completion = threading.Thread(target=lambda: inbox.complete("one", publish))
                    completion.start()
                    self.assertTrue(published.wait(2))
                    self.assertEqual(p["_read_frame"](peer.fileno())["kind"], terminal)
                    write("request", "two")
                    self.assertTrue(next_request_checked.wait(2))
                    release.set()
                    completion.join(2)
                    self.assertFalse(completion.is_alive())
                    self.assertEqual(inbox.requests.get(timeout=2)[0], "two")
                    self.assertEqual(inbox._current[0], "two")
                finally:
                    release.set()
                    peer.close()
                    reader.join(2)
                    worker.close()

    def test_i01_failed_publication_does_not_clear_owner(self):
        inbox = protocol()["RequestInbox"](-1)
        inbox._current = ("one", threading.Event())
        def broken():
            raise BrokenPipeError("partial terminal")
        with self.assertRaises(BrokenPipeError):
            inbox.complete("one", broken)
        self.assertEqual(inbox._current[0], "one")
        with self.assertRaises(RuntimeError):
            inbox.complete("other", lambda: None)

    def test_i01_true_overlap_still_refused(self):
        p = protocol()
        worker, peer = socket.socketpair()
        inbox = p["RequestInbox"](worker.fileno())
        reader = threading.Thread(target=inbox.run, daemon=True)
        reader.start()
        try:
            for request_id in ("one", "two"):
                p["_write_frame"](peer.fileno(), threading.Lock(), "request", request_id, {})
                item = inbox.requests.get(timeout=2)
                if request_id == "one":
                    self.assertEqual(item[0], "one")
                else:
                    self.assertIsInstance(item, ValueError)
        finally:
            peer.close()
            reader.join(2)
            worker.close()

    def test_i01_cancellation_keeps_exact_request_identity(self):
        for cancel_id in ("one", "wrong"):
            with self.subTest(cancel_id=cancel_id):
                p = protocol()
                worker, peer = socket.socketpair()
                inbox = p["RequestInbox"](worker.fileno())
                reader = threading.Thread(target=inbox.run, daemon=True)
                reader.start()
                try:
                    p["_write_frame"](peer.fileno(), threading.Lock(), "request", "one", {})
                    _, _, cancelled = inbox.requests.get(timeout=2)
                    p["_write_frame"](peer.fileno(), threading.Lock(), "cancel", cancel_id, None)
                    if cancel_id == "one":
                        self.assertTrue(cancelled.wait(2))
                    else:
                        self.assertIsInstance(inbox.requests.get(timeout=2), ValueError)
                        self.assertFalse(cancelled.is_set())
                finally:
                    peer.close()
                    reader.join(2)
                    worker.close()


if __name__ == "__main__":
    unittest.main()
