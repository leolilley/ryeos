"""Tests for the email agent e2e bundle — router, send, forward, graph, providers.

Unit tests for deterministic tools (router, send, forward) and structural
validation for the graph YAML and provider configs.
"""

import asyncio
import importlib.util
import json
import sys
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest
import yaml

from conftest import PROJECT_ROOT

# ---------------------------------------------------------------------------
# Bundle paths
# ---------------------------------------------------------------------------

EMAIL_BUNDLE = PROJECT_ROOT / "ryeos" / "bundles" / "email" / "ryeos_email"
TOOLS_DIR = EMAIL_BUNDLE / ".ai" / "tools" / "rye" / "email"
CONFIG_DIR = EMAIL_BUNDLE / ".ai" / "config" / "email"
DIRECTIVES_DIR = EMAIL_BUNDLE / ".ai" / "directives" / "rye" / "email"


def _load_tool(name: str):
    """Load a Python tool module from the email bundle."""
    tool_path = TOOLS_DIR / f"{name}.py"
    spec = importlib.util.spec_from_file_location(f"email_{name}", tool_path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

EMAIL_CONFIG = {
    "schema_version": "1.0.0",
    "provider": {"default": "campaign-kiwi"},
    "agent": {
        "inbox": "leo@agentkiwi.nz",
        "name": "Leo Lilley",
        "forward_to": "leo.lml.lilley@gmail.com",
    },
    "owner_emails": ["leo.lml.lilley@gmail.com", "leo@lilley.io"],
    "suppress_patterns": [
        "noreply@*",
        "no-reply@*",
        "notifications@*",
        "mailer-daemon@*",
        "postmaster@*",
        "auto-*@*",
    ],
}


# ===================================================================
# Test 1: Router — Owner email → auto_reply
# ===================================================================


class TestRouterOwnerEmail:
    """Send from an address listed in owner_emails → auto_reply."""

    def setup_method(self):
        self.router = _load_tool("router")

    def test_owner_email_routes_to_auto_reply(self):
        result = self.router.execute(
            {
                "from_address": "leo.lml.lilley@gmail.com",
                "to_address": "leo@agentkiwi.nz",
                "subject": "Hey, quick question",
                "body": "Can you check the deployment status?",
                "resolved_config": EMAIL_CONFIG,
            },
            "/tmp/fake-project",
        )
        assert result["success"] is True
        assert result["action"] == "auto_reply"
        assert result["sender_type"] == "owner"
        assert result["forward_to"] == "leo.lml.lilley@gmail.com"
        assert result["agent_inbox"] == "leo@agentkiwi.nz"

    def test_owner_email_case_insensitive(self):
        result = self.router.execute(
            {
                "from_address": "Leo.LML.Lilley@Gmail.com",
                "to_address": "leo@agentkiwi.nz",
                "subject": "Test",
                "body": "test",
                "resolved_config": EMAIL_CONFIG,
            },
            "/tmp/fake-project",
        )
        assert result["action"] == "auto_reply"
        assert result["sender_type"] == "owner"

    def test_second_owner_email(self):
        result = self.router.execute(
            {
                "from_address": "leo@lilley.io",
                "to_address": "leo@agentkiwi.nz",
                "subject": "From secondary",
                "body": "body",
                "resolved_config": EMAIL_CONFIG,
            },
            "/tmp/fake-project",
        )
        assert result["action"] == "auto_reply"
        assert result["sender_type"] == "owner"


# ===================================================================
# Test 2: Router — Unknown sender → forward
# ===================================================================


class TestRouterUnknownSender:
    """Send from an unlisted address → forward."""

    def setup_method(self):
        self.router = _load_tool("router")

    def test_unknown_sender_routes_to_forward(self):
        result = self.router.execute(
            {
                "from_address": "stranger@example.com",
                "to_address": "leo@agentkiwi.nz",
                "subject": "Business proposal",
                "body": "I'd like to discuss...",
                "resolved_config": EMAIL_CONFIG,
            },
            "/tmp/fake-project",
        )
        assert result["success"] is True
        assert result["action"] == "forward"
        assert result["sender_type"] == "unknown"
        assert result["forward_to"] == "leo.lml.lilley@gmail.com"

    def test_unknown_sender_context_includes_subject(self):
        result = self.router.execute(
            {
                "from_address": "someone@corp.com",
                "to_address": "leo@agentkiwi.nz",
                "subject": "Partnership opportunity",
                "body": "Hello",
                "resolved_config": EMAIL_CONFIG,
            },
            "/tmp/fake-project",
        )
        assert "Partnership opportunity" in result["context_summary"]


# ===================================================================
# Test 3: Router — Suppressed sender → suppress
# ===================================================================


class TestRouterSuppressed:
    """Send from noreply@*, notifications@*, etc. → suppress."""

    def setup_method(self):
        self.router = _load_tool("router")

    def test_noreply_suppressed(self):
        result = self.router.execute(
            {
                "from_address": "noreply@example.com",
                "to_address": "leo@agentkiwi.nz",
                "subject": "Your receipt",
                "body": "Thank you for your purchase",
                "resolved_config": EMAIL_CONFIG,
            },
            "/tmp/fake-project",
        )
        assert result["success"] is True
        assert result["action"] == "suppress"
        assert result["sender_type"] == "automated"

    def test_no_reply_hyphenated_suppressed(self):
        result = self.router.execute(
            {
                "from_address": "no-reply@github.com",
                "to_address": "leo@agentkiwi.nz",
                "subject": "New PR",
                "body": "body",
                "resolved_config": EMAIL_CONFIG,
            },
            "/tmp/fake-project",
        )
        assert result["action"] == "suppress"

    def test_notifications_suppressed(self):
        result = self.router.execute(
            {
                "from_address": "notifications@slack.com",
                "to_address": "leo@agentkiwi.nz",
                "subject": "New message",
                "body": "body",
                "resolved_config": EMAIL_CONFIG,
            },
            "/tmp/fake-project",
        )
        assert result["action"] == "suppress"

    def test_mailer_daemon_suppressed(self):
        result = self.router.execute(
            {
                "from_address": "mailer-daemon@mail.example.com",
                "to_address": "leo@agentkiwi.nz",
                "subject": "Delivery failure",
                "body": "body",
                "resolved_config": EMAIL_CONFIG,
            },
            "/tmp/fake-project",
        )
        assert result["action"] == "suppress"

    def test_auto_prefix_suppressed(self):
        result = self.router.execute(
            {
                "from_address": "auto-confirm@booking.com",
                "to_address": "leo@agentkiwi.nz",
                "subject": "Booking confirmed",
                "body": "body",
                "resolved_config": EMAIL_CONFIG,
            },
            "/tmp/fake-project",
        )
        assert result["action"] == "suppress"

    def test_suppress_takes_priority_over_owner(self):
        """If an owner email matches a suppress pattern, suppress wins (checked first)."""
        config = {**EMAIL_CONFIG, "owner_emails": ["noreply@myown.com"]}
        result = self.router.execute(
            {
                "from_address": "noreply@myown.com",
                "to_address": "leo@agentkiwi.nz",
                "subject": "test",
                "body": "body",
                "resolved_config": config,
            },
            "/tmp/fake-project",
        )
        assert result["action"] == "suppress"


# ===================================================================
# Test 4: Router — Thread reply → auto_reply
# ===================================================================


class TestRouterThreadReply:
    """Reply in existing conversation (thread_id set) → auto_reply."""

    def setup_method(self):
        self.router = _load_tool("router")

    def test_thread_reply_from_unknown_sender(self):
        result = self.router.execute(
            {
                "from_address": "stranger@example.com",
                "to_address": "leo@agentkiwi.nz",
                "subject": "Re: Our discussion",
                "body": "Following up on...",
                "thread_id": "thread_abc123",
                "resolved_config": EMAIL_CONFIG,
            },
            "/tmp/fake-project",
        )
        assert result["action"] == "auto_reply"
        assert result["sender_type"] == "known_thread"

    def test_thread_reply_no_thread_id_forwards(self):
        """Without thread_id, unknown sender still forwards."""
        result = self.router.execute(
            {
                "from_address": "stranger@example.com",
                "to_address": "leo@agentkiwi.nz",
                "subject": "Re: Our discussion",
                "body": "Following up on...",
                "resolved_config": EMAIL_CONFIG,
            },
            "/tmp/fake-project",
        )
        assert result["action"] == "forward"


# ===================================================================
# Test 5: Router — Missing config produces safe defaults
# ===================================================================


class TestRouterMissingConfig:
    """Router works with empty/minimal config."""

    def setup_method(self):
        self.router = _load_tool("router")

    def test_empty_config_forwards_everything(self):
        result = self.router.execute(
            {
                "from_address": "anyone@example.com",
                "to_address": "inbox@example.com",
                "subject": "Hello",
                "body": "body",
                "resolved_config": {},
            },
            "/tmp/fake-project",
        )
        assert result["action"] == "forward"
        assert result["forward_to"] is None

    def test_no_resolved_config_key(self):
        result = self.router.execute(
            {
                "from_address": "anyone@example.com",
                "to_address": "inbox@example.com",
                "subject": "Hello",
                "body": "body",
            },
            "/tmp/fake-project",
        )
        assert result["action"] == "forward"


# ===================================================================
# Test 6: Send tool — multi-step provider
# ===================================================================


class TestSendTool:
    """Send tool resolves provider and executes multi-step send."""

    def setup_method(self):
        self.send = _load_tool("send")

    def test_no_provider_returns_error(self):
        result = asyncio.run(
            self.send.execute(
                {
                    "to": "test@example.com",
                    "subject": "Test",
                    "body": "body",
                    "resolved_config": {"provider": {"default": None}},
                },
                "/tmp/fake-project",
            )
        )
        assert result["success"] is False
        assert "No email provider" in result["error"]

    def test_no_inbox_returns_error(self):
        result = asyncio.run(
            self.send.execute(
                {
                    "to": "test@example.com",
                    "subject": "Test",
                    "body": "body",
                    "resolved_config": {
                        "provider": {"default": "campaign-kiwi"},
                        "agent": {"inbox": None},
                    },
                },
                str(EMAIL_BUNDLE),
            )
        )
        assert result["success"] is False
        assert "No sending address" in result["error"]

    @patch("rye.actions.execute.ExecuteTool")
    def test_multistep_send_calls_create_approve_schedule(self, MockExecuteTool):
        """CK provider: create → approve → schedule."""
        mock_executor = AsyncMock()
        MockExecuteTool.return_value = mock_executor

        mock_executor.handle.side_effect = [
            {"status": "success", "data": {"email_id": "em_123"}},
            {"status": "success", "data": {"email_id": "em_123"}},
            {"status": "success", "data": {"email_id": "em_123", "message_id": "ses_abc"}},
        ]

        result = asyncio.run(
            self.send.execute(
                {
                    "to": "recipient@example.com",
                    "subject": "Hello",
                    "body": "Hi there",
                    "from": "leo@agentkiwi.nz",
                    "resolved_config": EMAIL_CONFIG,
                },
                str(EMAIL_BUNDLE),
            )
        )

        assert result["success"] is True
        assert result["email_id"] == "em_123"
        assert result["status"] == "sent"
        assert mock_executor.handle.call_count == 3

        # Verify the 3 MCP calls
        calls = mock_executor.handle.call_args_list
        assert "primary_email/create" in calls[0].kwargs["item_id"]
        assert "primary_email/approve" in calls[1].kwargs["item_id"]
        assert "scheduler/schedule" in calls[2].kwargs["item_id"]

    @patch("rye.actions.execute.ExecuteTool")
    def test_step_failure_returns_error(self, MockExecuteTool):
        mock_executor = AsyncMock()
        MockExecuteTool.return_value = mock_executor
        mock_executor.handle.return_value = {
            "status": "error",
            "error": "Rate limited",
        }

        result = asyncio.run(
            self.send.execute(
                {
                    "to": "test@example.com",
                    "subject": "Test",
                    "body": "body",
                    "from": "leo@agentkiwi.nz",
                    "resolved_config": EMAIL_CONFIG,
                },
                str(EMAIL_BUNDLE),
            )
        )

        assert result["success"] is False
        assert "Rate limited" in result["error"]


# ===================================================================
# Test 7: Forward tool
# ===================================================================


class TestForwardTool:
    """Forward tool fetches original email, builds forward body, sends."""

    def setup_method(self):
        self.forward = _load_tool("forward")

    def test_no_provider_returns_error(self):
        result = asyncio.run(
            self.forward.execute(
                {
                    "email_id": "em_456",
                    "classification": "unknown_sender",
                    "resolved_config": {"provider": {"default": None}},
                },
                "/tmp/fake-project",
            )
        )
        assert result["success"] is False
        assert "No email provider" in result["error"]

    def test_no_forward_to_returns_error(self):
        result = asyncio.run(
            self.forward.execute(
                {
                    "email_id": "em_456",
                    "classification": "unknown_sender",
                    "resolved_config": {
                        "provider": {"default": "campaign-kiwi"},
                        "agent": {"forward_to": None, "inbox": "leo@agentkiwi.nz"},
                    },
                },
                str(EMAIL_BUNDLE),
            )
        )
        assert result["success"] is False
        assert "No forward address" in result["error"]


# ===================================================================
# Test 8: Graph YAML structural validation
# ===================================================================


class TestHandleInboundGraph:
    """Validate handle_inbound.yaml structure matches state-graph spec."""

    def setup_method(self):
        with open(TOOLS_DIR / "handle_inbound.yaml") as f:
            self.graph = yaml.safe_load(f)

    def test_tool_type_is_graph(self):
        assert self.graph["tool_type"] == "graph"

    def test_executor_id(self):
        assert self.graph["executor_id"] == "rye/core/runtimes/state-graph/runtime"

    def test_start_node_exists(self):
        start = self.graph["config"]["start"]
        assert start in self.graph["config"]["nodes"]

    def test_all_edge_targets_exist(self):
        nodes = self.graph["config"]["nodes"]
        for name, node in nodes.items():
            next_val = node.get("next")
            if next_val is None:
                continue
            if isinstance(next_val, str):
                assert next_val in nodes, f"Node '{name}' targets non-existent '{next_val}'"
            elif isinstance(next_val, list):
                for edge in next_val:
                    assert edge["to"] in nodes, f"Node '{name}' targets non-existent '{edge['to']}'"

            on_error = node.get("on_error")
            if on_error:
                assert on_error in nodes, f"Node '{name}' on_error targets non-existent '{on_error}'"

    def test_required_inputs(self):
        required = self.graph["config_schema"]["required"]
        assert "email_id" in required
        assert "from_address" in required
        assert "subject" in required
        assert "body" in required

    def test_max_steps(self):
        assert self.graph["config"]["max_steps"] >= 4  # route + draft + send + done minimum

    def test_route_node_calls_router_tool(self):
        route = self.graph["config"]["nodes"]["route"]
        assert route["action"]["item_id"] == "tool:rye/email/router"

    def test_draft_reply_calls_directive(self):
        draft = self.graph["config"]["nodes"]["draft_reply"]
        assert draft["action"]["item_id"] == "directive:rye/email/draft_response"

    def test_draft_reply_has_error_edge(self):
        draft = self.graph["config"]["nodes"]["draft_reply"]
        assert draft["on_error"] == "forward_email"

    def test_send_reply_calls_tool(self):
        send = self.graph["config"]["nodes"]["send_reply"]
        assert send["action"]["item_id"] == "tool:rye/email/send"

    def test_forward_email_calls_tool(self):
        forward = self.graph["config"]["nodes"]["forward_email"]
        assert forward["action"]["item_id"] == "tool:rye/email/forward"

    def test_done_is_return_node(self):
        assert self.graph["config"]["nodes"]["done"]["type"] == "return"

    def test_routing_edges_cover_all_actions(self):
        route = self.graph["config"]["nodes"]["route"]
        next_edges = route["next"]
        targets = {e["to"] for e in next_edges if isinstance(e, dict) and "when" in e}
        assert "draft_reply" in targets
        assert "forward_email" in targets
        # The last edge (no when) is the default → done (suppress)
        default_edge = next_edges[-1]
        assert default_edge.get("to") == "done" or (isinstance(default_edge, str) and default_edge == "done")

    def test_category(self):
        assert self.graph["category"] == "rye/email"

    def test_tool_id_is_short_name(self):
        assert self.graph["tool_id"] == "handle_inbound"


# ===================================================================
# Test 9: Provider YAML validation
# ===================================================================


class TestProviderYAMLs:
    """Validate provider YAML structure and action mappings."""

    def test_campaign_kiwi_provider(self):
        with open(TOOLS_DIR / "providers" / "campaign-kiwi" / "campaign-kiwi.yaml") as f:
            provider = yaml.safe_load(f)

        assert provider["tool_id"] == "campaign-kiwi"
        assert provider["tool_type"] == "email_provider"
        assert provider["executor_id"] is None
        assert provider["mcp_server"] == "campaign-kiwi-remote"

        actions = provider["actions"]
        assert "send" in actions
        assert "get" in actions
        assert "list" in actions

        # Send is multi-step
        send = actions["send"]
        assert "steps" in send
        assert len(send["steps"]) == 3
        assert send["steps"][0]["type"] == "primary_email"
        assert send["steps"][0]["action"] == "create"
        assert send["steps"][1]["type"] == "primary_email"
        assert send["steps"][1]["action"] == "approve"
        assert send["steps"][2]["type"] == "scheduler"
        assert send["steps"][2]["action"] == "schedule"

    def test_gmail_provider(self):
        with open(TOOLS_DIR / "providers" / "gmail" / "gmail.yaml") as f:
            provider = yaml.safe_load(f)

        assert provider["tool_id"] == "gmail"
        assert provider["tool_type"] == "email_provider"
        assert provider["executor_id"] is None
        assert provider["mcp_server"] == "google-workspace"

        actions = provider["actions"]
        assert "send" in actions
        assert "get" in actions
        assert "list" in actions

        # Send is single-step (no "steps" key)
        send = actions["send"]
        assert "steps" not in send
        assert send["action"] == "send"
        assert send["type"] == "gmail"


# ===================================================================
# Test 10: Email config validation
# ===================================================================


class TestEmailConfig:
    """Validate bundled email.yaml config."""

    def setup_method(self):
        with open(CONFIG_DIR / "email.yaml") as f:
            self.config = yaml.safe_load(f)

    def test_schema_version(self):
        assert self.config["schema_version"] == "1.0.0"

    def test_provider_default_is_null(self):
        assert self.config["provider"]["default"] is None

    def test_agent_fields_are_null(self):
        assert self.config["agent"]["inbox"] is None
        assert self.config["agent"]["name"] is None
        assert self.config["agent"]["forward_to"] is None

    def test_owner_emails_empty(self):
        assert self.config["owner_emails"] == []

    def test_suppress_patterns_exist(self):
        patterns = self.config["suppress_patterns"]
        assert len(patterns) >= 5
        assert "noreply@*" in patterns
        assert "no-reply@*" in patterns
        assert "notifications@*" in patterns
        assert "mailer-daemon@*" in patterns
        assert "postmaster@*" in patterns


# ===================================================================
# Test 11: draft_response directive validation
# ===================================================================


class TestDraftResponseDirective:
    """Validate draft_response directive structure after update."""

    def setup_method(self):
        self.content = (DIRECTIVES_DIR / "draft_response.md").read_text()

    def test_thread_id_is_optional(self):
        assert 'name="thread_id" type="string" required="false"' in self.content

    def test_has_email_body_input(self):
        assert 'name="email_body"' in self.content

    def test_has_email_subject_input(self):
        assert 'name="email_subject"' in self.content

    def test_has_from_name_input(self):
        assert 'name="from_name"' in self.content

    def test_no_campaign_kiwi_references(self):
        assert "campaign-kiwi" not in self.content
        assert "campaign_kiwi" not in self.content

    def test_has_provider_agnostic_permissions(self):
        assert "rye/email/providers/*" in self.content


# ===================================================================
# Test 12: Build step params helper
# ===================================================================


class TestBuildStepParams:
    """Test the _build_step_params helper used by send tool."""

    def setup_method(self):
        self.send = _load_tool("send")

    def test_create_step_params(self):
        result = self.send._build_step_params(
            "create", "primary_email",
            {"to": "alice@example.com", "subject": "Hi", "body": "Hello"},
            "leo@agentkiwi.nz", "Kiwi", None,
        )
        assert result["to_emails"] == ["alice@example.com"]
        assert result["from_email"] == "leo@agentkiwi.nz"
        assert result["from_name"] == "Kiwi"
        assert result["subject"] == "Hi"
        assert result["body_text"] == "Hello"

    def test_approve_step_params(self):
        result = self.send._build_step_params(
            "approve", "primary_email",
            {"to": "x", "subject": "x", "body": "x"},
            "leo@agentkiwi.nz", "Kiwi", "em_123",
        )
        assert result == {"entity_id": "em_123"}

    def test_schedule_step_params(self):
        result = self.send._build_step_params(
            "schedule", "scheduler",
            {"to": "x", "subject": "x", "body": "x"},
            "leo@agentkiwi.nz", "Kiwi", "em_123",
        )
        assert result["email_ids"] == ["em_123"]
        assert result["email_type"] == "primary"
        assert result["scheduled_time"] == "immediate"
        assert result["dry_run"] is False
