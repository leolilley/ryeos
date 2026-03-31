"""Lightweight webhook trigger for ryeos-node.

Stdlib-only — no ryeos-engine dependency. Sends an HMAC-authenticated POST
to a ryeos-node /execute endpoint to trigger a webhook binding.

Usage:
    rye-webhook-trigger                          # uses env vars
    rye-webhook-trigger --hook-id wh_... --secret whsec_... --url https://...

Env vars:
    WEBHOOK_HOOK_ID   — webhook binding ID (wh_...)
    WEBHOOK_SECRET    — HMAC secret (whsec_...)
    RYEOS_NODE_URL    — node base URL (https://...)
    WEBHOOK_PARAMS    — optional JSON object of parameters
"""

import argparse
import hashlib
import hmac
import json
import sys
import time
import uuid
from urllib.request import Request, urlopen
from urllib.error import HTTPError, URLError
import os


def trigger(
    hook_id: str,
    secret: str,
    node_url: str,
    parameters: dict | None = None,
    timeout: int = 300,
) -> dict:
    """Send HMAC-authenticated webhook trigger to ryeos-node.

    Returns the parsed JSON response body.
    Raises on HTTP errors or connection failures.
    """
    url = f"{node_url.rstrip('/')}/execute"
    timestamp = str(int(time.time()))
    delivery_id = uuid.uuid4().hex

    body = {"hook_id": hook_id}
    if parameters:
        body["parameters"] = parameters

    body_bytes = json.dumps(body).encode("utf-8")

    # HMAC-SHA256 over "timestamp.body"
    signed = timestamp.encode() + b"." + body_bytes
    signature = "sha256=" + hmac.new(
        secret.encode(), signed, hashlib.sha256,
    ).hexdigest()

    req = Request(
        url,
        data=body_bytes,
        headers={
            "Content-Type": "application/json",
            "X-Webhook-Timestamp": timestamp,
            "X-Webhook-Signature": signature,
            "X-Webhook-Delivery-Id": delivery_id,
        },
        method="POST",
    )

    try:
        with urlopen(req, timeout=timeout) as resp:
            return json.loads(resp.read().decode("utf-8"))
    except HTTPError as e:
        error_body = e.read().decode("utf-8", errors="replace")
        try:
            error_detail = json.loads(error_body)
        except (json.JSONDecodeError, ValueError):
            error_detail = error_body
        raise SystemExit(
            f"HTTP {e.code}: {json.dumps(error_detail, indent=2) if isinstance(error_detail, dict) else error_detail}"
        )
    except URLError as e:
        raise SystemExit(f"Connection error: {e.reason}")


def main():
    parser = argparse.ArgumentParser(
        description="Trigger a ryeos-node webhook binding",
    )
    parser.add_argument(
        "--hook-id",
        default=os.environ.get("WEBHOOK_HOOK_ID"),
        help="Webhook binding ID (default: $WEBHOOK_HOOK_ID)",
    )
    parser.add_argument(
        "--secret",
        default=os.environ.get("WEBHOOK_SECRET"),
        help="HMAC secret (default: $WEBHOOK_SECRET)",
    )
    parser.add_argument(
        "--url",
        default=os.environ.get("RYEOS_NODE_URL"),
        help="Node base URL (default: $RYEOS_NODE_URL)",
    )
    parser.add_argument(
        "--params",
        default=os.environ.get("WEBHOOK_PARAMS"),
        help='JSON parameters (default: $WEBHOOK_PARAMS, e.g. \'{"key": "value"}\')',
    )
    parser.add_argument(
        "--timeout",
        type=int,
        default=300,
        help="Request timeout in seconds (default: 300)",
    )

    args = parser.parse_args()

    if not args.hook_id:
        parser.error("--hook-id or WEBHOOK_HOOK_ID env var required")
    if not args.secret:
        parser.error("--secret or WEBHOOK_SECRET env var required")
    if not args.url:
        parser.error("--url or RYEOS_NODE_URL env var required")

    parameters = None
    if args.params:
        try:
            parameters = json.loads(args.params)
        except json.JSONDecodeError as e:
            parser.error(f"Invalid --params JSON: {e}")

    result = trigger(
        hook_id=args.hook_id,
        secret=args.secret,
        node_url=args.url,
        parameters=parameters,
        timeout=args.timeout,
    )

    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
