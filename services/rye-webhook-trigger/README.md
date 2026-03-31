# rye-webhook-trigger

Lightweight, stdlib-only CLI for triggering ryeos-node webhook bindings. No engine dependency — Docker image is ~50MB vs ~300MB with ryeos-engine.

## Install

```bash
pip install rye-webhook-trigger
```

## Usage

```bash
# Via env vars (recommended for cron/Docker)
export WEBHOOK_HOOK_ID=wh_abc123...
export WEBHOOK_SECRET=whsec_def456...
export RYEOS_NODE_URL=https://your-node.up.railway.app

rye-webhook-trigger

# Via CLI args
rye-webhook-trigger \
  --hook-id wh_abc123 \
  --secret whsec_def456 \
  --url https://your-node.up.railway.app

# With parameters
rye-webhook-trigger --params '{"date": "2026-03-30"}'
```

## Docker

```dockerfile
FROM python:3.12-slim
WORKDIR /app
COPY . .
RUN pip install --no-cache-dir .
ENTRYPOINT ["rye-webhook-trigger"]
```

## As a library

```python
from rye_webhook_trigger import trigger

result = trigger(
    hook_id="wh_abc123",
    secret="whsec_def456",
    node_url="https://your-node.up.railway.app",
    parameters={"date": "2026-03-30"},
)
```
