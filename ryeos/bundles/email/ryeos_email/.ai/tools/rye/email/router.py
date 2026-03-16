# rye:signed:2026-03-16T09:53:44Z:4a2f7cfeaef06151201b1dc9b14ce7742ec29d83d209367692b2dcf7272df81e:bDMZmoQFfHmYn4ysPt2b2_r_qBmAZGFQmLDDz0czeRzTMpeDIoswHJrTxgwrXolHAM6k93tSH_agASHbHkh0AA==:4b987fd4e40303ac
"""Deterministic email router — classifies inbound emails without LLM."""

import argparse
import json
import sys
from fnmatch import fnmatch
from pathlib import Path

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/function"
__category__ = "rye/email"
__tool_description__ = "Route inbound emails — suppress, auto-reply, or forward"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "from_address": {
            "type": "string",
            "description": "Sender email address",
        },
        "to_address": {
            "type": "string",
            "description": "Recipient email address (our inbox)",
        },
        "subject": {
            "type": "string",
            "description": "Email subject line",
        },
        "body": {
            "type": "string",
            "description": "Email body text",
        },
        "thread_id": {
            "type": "string",
            "description": "Thread ID if this is a reply",
        },
        "in_reply_to": {
            "type": "string",
            "description": "In-Reply-To header value",
        },
    },
    "required": ["from_address", "to_address", "subject", "body"],
}

CONFIG_RESOLVE = {
    "path": "email/email.yaml",
    "mode": "deep_merge",
}


def execute(params: dict, project_path: str) -> dict:
    """Route an inbound email based on deterministic rules."""
    config = params.get("resolved_config", {})

    from_address = params["from_address"].lower()
    to_address = params.get("to_address", "")
    subject = params.get("subject", "")
    thread_id = params.get("thread_id")

    suppress_patterns = config.get("suppress_patterns", [])
    owner_emails = [e.lower() for e in config.get("owner_emails", [])]
    agent_config = config.get("agent", {})
    forward_to = agent_config.get("forward_to")
    agent_inbox = agent_config.get("inbox")
    agent_name = agent_config.get("name")

    # 1. Check suppress patterns
    for pattern in suppress_patterns:
        if fnmatch(from_address, pattern.lower()):
            return {
                "success": True,
                "action": "suppress",
                "sender_type": "automated",
                "forward_to": None,
                "agent_inbox": agent_inbox,
                "context_summary": f"Suppressed: {from_address} matches pattern '{pattern}'",
            }

    # 2. Check owner emails
    if from_address in owner_emails:
        return {
            "success": True,
            "action": "auto_reply",
            "sender_type": "owner",
            "forward_to": forward_to,
            "agent_inbox": agent_inbox,
            "context_summary": f"Owner email from {from_address}",
        }

    # 3. Check thread_id (reply in existing conversation)
    if thread_id:
        return {
            "success": True,
            "action": "auto_reply",
            "sender_type": "known_thread",
            "forward_to": forward_to,
            "agent_inbox": agent_inbox,
            "context_summary": f"Reply in thread {thread_id} from {from_address}",
        }

    # 4. Unknown sender → forward
    return {
        "success": True,
        "action": "forward",
        "sender_type": "unknown",
        "forward_to": forward_to,
        "agent_inbox": agent_inbox,
        "context_summary": f"Unknown sender: {from_address}, subject: {subject}",
    }


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--project-path", required=True)
    args = parser.parse_args()
    params = json.loads(sys.stdin.read())
    result = execute(params, args.project_path)
    print(json.dumps(result))
