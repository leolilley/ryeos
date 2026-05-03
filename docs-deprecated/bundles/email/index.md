```yaml
id: bundle-email
title: "Email Bundle"
description: Provider-agnostic email processing — inbound routing, sending, forwarding, and LLM-powered drafting
category: bundles
tags: [bundle, email, ryeos-email, inbound, outbound, provider, graph]
version: "1.0.0"
```

# Email Bundle

**Package:** `ryeos-email` · **Install:** `pip install ryeos[email]`
**Namespace:** `rye/email/`

Provider-agnostic email processing for AI agents. Rye OS is the intelligence layer — it classifies, decides, drafts, and routes. The mechanical work (MIME parsing, spam filtering, raw transport) is handled by external services. The email provider (Campaign Kiwi, Gmail, etc.) stores emails and provides sending via MCP.

---

## Architecture

Three layers, each with a clear responsibility:

| Layer | Responsibility | Examples |
| --- | --- | --- |
| **Transport** | MIME parsing, spam filtering, raw delivery | AWS Lambda, SES |
| **Provider** | Email storage, sending API, MCP tools | Campaign Kiwi, Gmail |
| **Intelligence** | Classification, routing, drafting, digests | Rye OS (this bundle) |

The bundle is **provider-agnostic**. Campaign Kiwi is one provider; Gmail is another. Tools resolve the active provider at runtime from config and dispatch to the correct MCP server.

---

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
│   └── daily_digest.md                     # LLM — generate daily stats & send
└── knowledge/rye/email/
    └── email-directives.md                 # Internal reference documentation
```

### Tools vs Directives

Mechanical operations are **tools** — they execute deterministically without LLM involvement. Only operations requiring intelligence remain as **directives**:

| Item | Type | Why |
| --- | --- | --- |
| `router.py` | tool | Pattern-matching rules, no LLM needed |
| `send.py` | tool | Provider dispatch, no LLM needed |
| `forward.py` | tool | Template + provider dispatch, no LLM needed |
| `handle_inbound.yaml` | tool (graph) | Deterministic state machine orchestrating tools |
| `draft_response.md` | directive | Needs LLM to compose contextual reply |
| `daily_digest.md` | directive | Needs LLM to summarize and generate stats |

---

## Tools

### `router` — Deterministic Classifier

**Item ID:** `rye/email/router`

Classifies inbound emails without LLM overhead using pattern matching against `email.yaml` config. Returns a routing action.

#### Parameters

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `from_address` | string | ✅ | Sender email address |
| `to_address` | string | ✅ | Recipient email address |
| `subject` | string | ✅ | Email subject line |
| `body` | string | ✅ | Email body |
| `thread_id` | string | ❌ | Thread ID for reply detection |
| `in_reply_to` | string | ❌ | In-Reply-To header |

#### Actions

| Action | Trigger | Effect |
| --- | --- | --- |
| `auto_reply` | Sender is in `owner_emails` | Draft and send a reply via LLM |
| `forward` | Unknown sender, not suppressed | Forward to owner's private address |
| `suppress` | Sender matches `suppress_patterns` | No action taken |

### `send` — Provider-Agnostic Send

**Item ID:** `rye/email/send`

Sends an email through the active provider. Resolves the provider at runtime and walks the required steps (single-step for Gmail, multi-step for Campaign Kiwi).

#### Parameters

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `to` | string | ✅ | Recipient email address |
| `subject` | string | ✅ | Email subject |
| `body` | string | ✅ | Email body |

### `forward` — Forward with Context

**Item ID:** `rye/email/forward`

Forwards an email to the configured `forward_to` address with classification context.

#### Parameters

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `email_id` | string | ✅ | ID of the email to forward |
| `forward_to` | string | ❌ | Override destination (defaults to config) |
| `classification` | string | ❌ | Classification label for context |

### `handle_inbound` — State Graph

**Item ID:** `rye/email/handle_inbound`
**Type:** `graph` (state graph tool)

Orchestrates inbound email processing through deterministic routing with conditional edges. This is a YAML state graph, not a directive — only one node (`draft_reply`) invokes an LLM.

#### Parameters

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `email_id` | string | ✅ | Email identifier |
| `from_address` | string | ✅ | Sender address |
| `to_address` | string | ✅ | Recipient address |
| `subject` | string | ✅ | Email subject |
| `body` | string | ✅ | Email body |
| `thread_id` | string | ❌ | Thread ID for reply detection |
| `in_reply_to` | string | ❌ | In-Reply-To header |

#### Graph Flow

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

#### Nodes

| Node | Executes | Edges |
| --- | --- | --- |
| `route` | `tool: rye/email/router` | → `draft_reply` (auto_reply), `forward_email` (forward), `done` (suppress) |
| `draft_reply` | `directive: rye/email/draft_response` | → `send_reply` · on_error → `forward_email` |
| `send_reply` | `tool: rye/email/send` | → `done` |
| `forward_email` | `tool: rye/email/forward` | → `done` |
| `done` | return (terminal) | — |

---

## Directives

### `draft_response`

**Item ID:** `rye/email/draft_response`

LLM-powered directive that drafts a contextual reply using the email thread.

#### Parameters

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `email_body` | string | ✅ | Body of the email to reply to |
| `email_subject` | string | ✅ | Subject of the email |
| `from_name` | string | ✅ | Sender name/address |
| `thread_id` | string | ❌ | Thread ID for context |

#### Output

Returns `draft_body` and `draft_subject` in the result.

### `daily_digest`

**Item ID:** `rye/email/daily_digest`

LLM-powered directive that generates daily email statistics and sends a digest.

---

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

---

## Configuration

`email.yaml` with bundle defaults (all null — user overrides in project space):

```yaml
schema_version: "1.0.0"

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

User sets overrides in project-space config (`.ai/config/email/email.yaml`). Three-tier resolution applies: project → user → system (deep merge).

---

## Webhook Integration

Inbound emails arrive via webhook. The webhook binding uses `item_type: "tool"` to trigger the `handle_inbound` graph via `/execute`. Payloads are HMAC-signed and replay-protected.

```
Webhook → /execute → handle_inbound.yaml (graph) → route → act
```

---

## Examples

```python
# Send an email (tool, not directive)
rye_execute(item_type="tool", item_id="rye/email/send",
    parameters={"to": "user@example.com", "subject": "Hello", "body": "..."})

# Forward an email (tool)
rye_execute(item_type="tool", item_id="rye/email/forward",
    parameters={"email_id": "...", "classification": "unknown_sender"})

# Handle inbound (graph tool — typically triggered by webhook)
rye_execute(item_type="tool", item_id="rye/email/handle_inbound",
    parameters={"email_id": "...", "from_address": "...", "to_address": "...",
                "subject": "...", "body": "..."})

# Draft a response (directive — requires LLM)
rye_execute(item_type="directive", item_id="rye/email/draft_response",
    parameters={"email_body": "...", "email_subject": "...",
                "from_name": "...", "thread_id": "..."})

# Daily digest (directive — requires LLM)
rye_execute(item_type="directive", item_id="rye/email/daily_digest",
    parameters={})
```
