#!/usr/bin/env python3
"""Deterministic secret-free Chat Completions provider for image qualification."""

import json
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

REQUIRED_CONTEXT = "The sky is blue on a clear day."


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):  # noqa: N802 - BaseHTTPRequestHandler API
        if self.path != "/health":
            self.send_error(404)
            return
        self.send_response(200)
        self.send_header("content-length", "2")
        self.end_headers()
        self.wfile.write(b"ok")

    def do_POST(self):  # noqa: N802 - BaseHTTPRequestHandler API
        length = int(self.headers.get("content-length", "0"))
        payload = json.loads(self.rfile.read(length) or b"{}")
        serialized = json.dumps(payload)
        if "SHUTDOWN_PROBE" in serialized:
            with open("/tmp/mock-shutdown-request", "w", encoding="utf-8") as marker:
                marker.write("received\n")
            time.sleep(300)
            return
        if REQUIRED_CONTEXT not in serialized:
            encoded = b'{"error":"required composed knowledge context missing"}'
            self.send_response(422)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(encoded)))
            self.end_headers()
            self.wfile.write(encoded)
            return
        if payload.get("stream"):
            frames = [
                {"choices": [{"delta": {"content": "Context was loaded."}}]},
                {"choices": [{"delta": {}, "finish_reason": "stop"}]},
            ]
            body = "".join(f"data: {json.dumps(frame)}\n\n" for frame in frames)
            body += "data: [DONE]\n\n"
            encoded = body.encode()
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
        else:
            encoded = json.dumps(
                {
                    "choices": [
                        {
                            "message": {
                                "role": "assistant",
                                "content": "Context was loaded.",
                            },
                            "finish_reason": "stop",
                        }
                    ],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1},
                }
            ).encode()
            self.send_response(200)
            self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(encoded)))
        self.end_headers()
        self.wfile.write(encoded)

    def log_message(self, _format, *_args):
        return


if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", 8000), Handler).serve_forever()
