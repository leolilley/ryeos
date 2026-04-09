<!-- rye:signed:2026-04-09T00:09:13Z:f27b589a046ed3d154f58f1f471911171aa18a2accde2ba5f97b2abae5093dc1:pbfQ9ArEekZaEPXfWtYS_qH7uG8tzW6wv4M0YpIp6hZaKVbkLKuXY-d-a0_khixQXaMJkczICH0ffTe1UYdaDQ:4b987fd4e40303ac -->
```yaml
name: email-directives
title: Email Bundle — Tools, Directives & Provider Abstraction
entry_type: reference
category: rye/email
version: "2.0.0"
author: leo
created_at: 2026-03-16T00:00:00Z
tags:
  - email
  - inbound
  - outbound
  - forward
  - digest
  - graph
  - provider
references: []
```

# Email Bundle — Tools, Directives & Provider Abstraction

## Architecture Overview

ryeos is the intelligence layer for email. The mechanical work — MIME parsing, spam filtering, raw transport — is handled by external services (Lambda, SES, etc.). The email provider (Campaign Kiwi, Gmail, etc.) stores emails and provides sending via MCP. ryeos sits on top: it classifies, decides, drafts, and routes.

The email bundle is **provider-agnostic**. Campaign Kiwi is one provider; Gmail is another. Tools resolve the active provider at runtime from config and dispatch to the correct MCP server.

## Bundle Structure

```
.ai/
├── config/email/
│   └── email.yaml                          # Agent & provider config (bundle defaults)
├── tools/rye/email/
│   ├── router.py                           # Deterministic inbound classifier
│   ├── send.py                             # Send via resolved provider
│   ├── forward.py                          # Forward with agent context
│   ├── handle_inbound.yaml                 # State graph — inbound processing
│   └── providers/
│       ├── campaign-kiwi/campaign-kiwi.yaml
│       └── gmail/gmail.yaml
├── directives/rye/email/
│   ├── draft_response.md                   # LLM — draft a reply using thread context
│   ├── daily_digest.md                     # LLM — generate daily stats & send
│   ├── reply.md                            # (legacy, superseded by tools)
│   ├── handle_inbound.md                   # (legacy, superseded by graph tool)
│   ├── send.md                             # (legacy, superseded by send.py)
│   └── forward.md                          # (legacy, superseded by forward.py)
└── knowledge/rye/email/
    └── email-directives.md                 # This file
```

### Tools vs Directives

Mechanical operations are now **tools** — they execute deterministically without LLM involvement. Only operations requiring intelligence remain as **directives**:

| Item               | Type      | Why                                            |
| ------------------ | --------- | ---------------------------------------------- |
| `router.py`        | tool      | Pattern-matching rules, no LLM needed          |
| `send.py`          | tool      | Provider dispatch, no LLM needed               |
| `forward.py`       | tool      | Template + provider dispatch, no LLM needed    |
| `handle_inbound`   | tool/graph| Deterministic state machine orchestrating tools |
| `draft_response`   | directive | Needs LLM to compose contextual reply          |
| `daily_digest`     | directive | Needs LLM to summarize and generate stats      |

## handle_inbound Graph

`handle_inbound.yaml` is a **state graph** (`tool_type: graph`), not a directive. It orchestrates inbound email processing through deterministic routing with conditional edges.

### Graph Flow

```
                  ┌─────────┐
                  │  route   │  (router.py — deterministic classification)
                  └────┬─────┘
                       │
         ┌─────────────┼──────────────┐
         │             │              │
    auto_reply      forward       suppress
         │             │              │
    ┌────▼─────┐  ┌────▼──────┐  ┌───▼───┐
    │draft_reply│  │forward_   │  │ done  │
    │(directive)│  │email(tool)│  └───────┘
    └────┬─────┘  └────┬──────┘
         │on_error──►  │
    ┌────▼─────┐       │
    │send_reply│       │
    │  (tool)  │       │
    └────┬─────┘       │
         │             │
         └──────┬──────┘
                ▼
              done
```

### Nodes

| Node            | Executes                       | Edges                                       |
| --------------- | ------------------------------ | ------------------------------------------- |
| `route`         | `tool: rye/email/router`       | → `draft_reply` (auto_reply), `forward_email` (forward), `done` (suppress) |
| `draft_reply`   | `directive: rye/email/draft_response` | → `send_reply` · on_error → `forward_email` |
| `send_reply`    | `tool: rye/email/send`         | → `done`                                    |
| `forward_email` | `tool: rye/email/forward`      | → `done`                                    |
| `done`          | return (terminal)              | —                                           |

Only `draft_reply` invokes an LLM (via the `draft_response` directive). All other nodes are pure tool execution.

## Provider Abstraction

Follows the same pattern as `rye/agent/providers/`. Each provider YAML defines:

- **`mcp_server`** — which MCP server to call
- **`actions`** — mappings for `send`, `get`, `list` with `params_map` for field translation

### Multi-Step vs Single-Step

Campaign Kiwi requires a multi-step send (create → approve → schedule):

```yaml
actions:
  send:
    steps:
      - tool: primary_email.create
        params_map: { to: to, subject: subject, body: body, ... }
      - tool: primary_email.approve
        params_map: { email_id: "$prev.email_id" }
      - tool: scheduler.schedule
        params_map: { email_id: "$prev.email_id", email_type: primary }
```

Gmail is single-step:

```yaml
actions:
  send:
    tool: gmail.send
    params_map: { to: to, subject: subject, body: body, from: from }
```

The send tool resolves the active provider at runtime and walks the steps transparently.

## Configuration

`email.yaml` with bundle defaults (all null — user overrides in project space):

```yaml
provider:
  default: null          # "campaign-kiwi", "gmail", etc.

agent:
  inbox: null            # Agent's email address
  name: null             # Agent display name
  forward_to: null       # Owner's private address for forwarded mail

owner_emails: []         # Addresses that get auto_reply treatment

suppress_patterns:       # Glob patterns for automated senders
  - "noreply@*"
  - "no-reply@*"
  - "notifications@*"
  - "mailer-daemon@*"
  - "postmaster@*"
  - "auto-*@*"
```

User sets their overrides in project-space config (`.ai/config/email/email.yaml`). Three-tier resolution applies: project → user → system (deep merge).

## Webhook Integration

Inbound emails arrive via webhook. The webhook binding uses `item_type: "tool"` to trigger the `handle_inbound` graph via `/execute`. Payloads are HMAC-signed and replay-protected.

```
Webhook → /execute → handle_inbound.yaml (graph) → route → act
```

## Invocation Examples

```python
# Send an email (tool, not directive)
rye_execute(item_id="rye/email/send",
    parameters={"to": "user@example.com", "subject": "Hello", "body": "..."})

# Forward an email (tool)
rye_execute(item_id="rye/email/forward",
    parameters={"email_id": "...", "classification": "unknown_sender"})

# Handle inbound (graph tool — typically triggered by webhook)
rye_execute(item_id="rye/email/handle_inbound",
    parameters={"email_id": "...", "from_address": "...", "to_address": "...",
                "subject": "...", "body": "..."})

# Draft a response (directive — requires LLM)
rye_execute(item_id="rye/email/draft_response",
    parameters={"email_body": "...", "email_subject": "...",
                "from_name": "...", "thread_id": "..."})

# Daily digest (directive — requires LLM)
rye_execute(item_id="rye/email/daily_digest",
    parameters={})
```
