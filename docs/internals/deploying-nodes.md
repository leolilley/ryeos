```yaml
id: deploying-nodes
title: "Deploying Execution Nodes — Modal, Render, Local"
description: Deployment guide for ryeos-node across three targets, scoping the work to get CPU execution nodes running for directive execution and tool dispatch
category: internals
tags: [deployment, nodes, modal, render, local, operations]
version: "1.0.0"
status: implemented
```

# Deploying Execution Nodes — Modal, Render, Local

> **Status:** Implemented — init module, Dockerfile, Modal/Render/local configs all landed. GPU nodes and the completions server come later.

## Goal

Get `ryeos-node` running on three targets so CPU execution nodes can:

- Accept signed requests and webhook triggers
- Execute directives (fork threads) and tools (inline)
- Dispatch to remote GPU nodes when routing tools are present
- Use an external LLM provider (Anthropic/OpenAI) initially, swap to sovereign inference later

---

## What Exists

| Component                 | Status                                                                     |
| ------------------------- | -------------------------------------------------------------------------- |
| `ryeos-node` server       | ✅ FastAPI server with /execute, /push, /status, webhooks, registry        |
| `modal_app.py`            | ✅ Working Modal deployment (volume-backed /cas)                           |
| `node.yaml` config        | ✅ Identity, hardware, features, limits, coordination                      |
| Config schema validation  | ✅ `node.config-schema.yaml`                                               |
| `/status` endpoint        | ✅ Reports capabilities, hardware, load                                    |
| Route tool + status cache | ✅ Capability-based dispatch with topology config                          |
| `cluster/topology.yaml`   | ✅ Routing policy (strategy, thresholds, TTLs)                             |
| Authorized key auth       | ✅ Ed25519 signed requests, TOML key files                                 |
| First-boot init           | ✅ `ryeos_node/init.py` — idempotent, generates keys + scaffolds node.yaml |
| Dockerfile                | ✅ Multi-target container build                                            |
| `render.yaml`             | ✅ One-click Render blueprint                                              |
| Local run script          | ✅ `run.sh` — defaults to `~/.ai/node`                                  |

---

## Node Space — The Deployment Config

Every node's identity and configuration lives in its **node space** — the `.ai/` tree under `<node_config>/` (defaults to `<cas_base_path>/config/`).

```
<cas_base_path>/
├── config/                          ← node_config root
│   ├── .ai/
│   │   └── config/
│   │       └── node/
│   │           └── node.yaml        ← node manifest
│   └── authorized_keys/
│       └── <fingerprint>.toml       ← who can execute here
├── signing/                         ← node's Ed25519 keypair
│   ├── id_ed25519
│   └── id_ed25519.pub
├── <user-fp-1>/                     ← per-user CAS storage
│   ├── .ai/objects/
│   ├── cache/
│   └── executions/
└── <user-fp-2>/
    └── ...
```

`node.yaml` replaces scattered env vars. Instead of setting `RYE_REMOTE_NAME`, `MAX_CONCURRENT`, `SIGNING_KEY_DIR` etc across different platforms, you write one file:

```yaml
identity:
  name: snap-track-cpu
  signing_key_dir: /cas/signing
hardware:
  gpus: 0
  memory_gb: 2
features:
  registry: false
  webhooks: true
limits:
  max_concurrent: 8
  max_request_bytes: 52428800
  max_user_storage_bytes: 1073741824
coordination:
  type: asyncio
```

Settings loads this via `@model_validator(mode='before')` — node.yaml values are defaults, env vars still override.

---

## Target 1: Modal

### What changes from current `modal_app.py`

| Change                          | Why                                                        |
| ------------------------------- | ---------------------------------------------------------- |
| Rename `remote_server` → `node` | Matches rename                                             |
| Add first-boot init hook        | Generate signing key + scaffold node space on empty volume |
| Add `node.yaml` to volume       | Replace env var sprawl                                     |
| Bump `max_inputs`               | Current `max_inputs=1` is conservative                     |
| Add health check                | Modal supports `@app.function(keep_warm=1)` for always-on  |

### `modal_app.py` updates

```python
app = modal.App("ryeos-node")
cas_volume = modal.Volume.from_name("ryeos-node-cas", create_if_missing=True)

image = (
    modal.Image.debian_slim(python_version="3.12")
    .pip_install(
        "ryeos>=0.1.20",
        "fastapi>=0.109.0",
        "uvicorn[standard]>=0.27.0",
        "pydantic-settings>=2.1.0",
        "pyyaml>=6.0",
    )
    .add_local_dir("ryeos_node", remote_path="/app/ryeos_node", copy=True)
    .env({
        "CAS_BASE_PATH": "/cas",
        "PYTHONPATH": "/app",
        "RYE_KERNEL_PYTHON": "/usr/local/bin/python3",
    })
)

@app.function(
    image=image,
    volumes={"/cas": cas_volume},
    timeout=300,
    keep_warm=1,
)
@modal.concurrent(max_inputs=8)
@modal.asgi_app()
def node():
    from ryeos_node.init import ensure_node_space
    ensure_node_space("/cas")

    from ryeos_node.server import app as fastapi_app
    return fastapi_app
```

### Provisioning

```bash
# First deploy — creates volume, generates keys
modal deploy modal_app.py

# Add authorized key via modal shell
modal shell modal_app.py
mkdir -p /cas/config/authorized_keys
cat > /cas/config/authorized_keys/<your-fp>.toml << 'EOF'
owner = "leo"
capabilities = ["rye.*"]
EOF

# Write node.yaml
mkdir -p /cas/config/.ai/config/node
cat > /cas/config/.ai/config/node/node.yaml << 'EOF'
identity:
  name: modal-cpu-1
hardware:
  gpus: 0
  memory_gb: 2
features:
  webhooks: true
limits:
  max_concurrent: 8
coordination:
  type: modal
EOF
```

---

## Target 2: Render

### What's needed

| Artifact        | Purpose                                               |
| --------------- | ----------------------------------------------------- |
| `Dockerfile`    | Standard container build                              |
| `render.yaml`   | Blueprint for one-click deploy                        |
| Persistent disk | Mounted at `/cas` for CAS + signing key + node config |

### Dockerfile

```dockerfile
FROM python:3.12-slim
WORKDIR /app

RUN pip install --no-cache-dir \
    "ryeos>=0.1.20" \
    "fastapi>=0.109.0" \
    "uvicorn[standard]>=0.27.0" \
    "pydantic-settings>=2.1.0" \
    "pyyaml>=6.0"

COPY ryeos_node/ /app/ryeos_node/

ENV CAS_BASE_PATH=/cas \
    PYTHONPATH=/app \
    RYE_KERNEL_PYTHON=/usr/local/bin/python3

EXPOSE 8000

CMD ["sh", "-c", "python -m ryeos_node.init /cas && uvicorn ryeos_node.server:app --host 0.0.0.0 --port ${PORT:-8000}"]
```

### render.yaml

```yaml
services:
  - type: web
    name: ryeos-node
    runtime: docker
    dockerfilePath: ./ryeos-node/Dockerfile
    dockerContext: ./ryeos-node
    healthCheckPath: /health
    disk:
      name: cas-storage
      mountPath: /cas
      sizeGB: 10
    envVars:
      - key: CAS_BASE_PATH
        value: /cas
```

### Provisioning

Render's persistent disk survives redeploys. First deploy generates signing key automatically (via init module). Authorized keys added via Render shell or baked into the Docker image for known keys.

---

## Target 3: Local

### Bare metal

```bash
# Install
pip install ryeos "fastapi[standard]" pydantic-settings

# Init node space (creates signing key, scaffolds node.yaml)
python -m ryeos_node.init ~/.ai/node

# Add your own key as authorized
cp ~/.ai/signing/id_ed25519.pub ~/.ai/node/config/authorized_keys/$(rye identity fingerprint).toml

# Edit node.yaml
vim ~/.ai/node/config/.ai/config/node/node.yaml

# Run
CAS_BASE_PATH=~/.ai/node uvicorn ryeos_node.server:app --port 8000
```

### Docker compose

```yaml
services:
  ryeos-node:
    build: ./ryeos-node
    ports:
      - "8000:8000"
    volumes:
      - cas-data:/cas
    environment:
      - CAS_BASE_PATH=/cas

volumes:
  cas-data:
```

### `run.sh` convenience script

```bash
#!/bin/bash
# Quick-start a local ryeos-node
CAS=${CAS_BASE_PATH:-$HOME/.ai/node}
export CAS_BASE_PATH="$CAS"

python -m ryeos_node.init "$CAS"
uvicorn ryeos_node.server:app --host 0.0.0.0 --port "${PORT:-8000}"
```

---

## Wiring It Up for Snap-Track

Once a CPU node is running on any target, the snap-track setup is:

1. **Push the project** — `rye push` syncs `.ai/` (directives, tools, config) to the node
2. **Configure provider** — `.ai/config/agent/agent.yaml` points at Anthropic initially
3. **Trigger** — webhook or signed request to `/execute` with `snap-track/add-show`
4. **Later** — swap provider to self-hosted completions server when GPU nodes are deployed

The CPU node handles directive execution, browser automation, database writes. LLM calls go through the provider path (same as local development). GPU routing comes later — the node is already wired for it via the route tool and topology config.

---

## Implementation Files

| Component        | File                            |
| ---------------- | ------------------------------- |
| Init module      | `ryeos-node/ryeos_node/init.py` |
| Modal deployment | `ryeos-node/modal_app.py`       |
| Dockerfile       | `ryeos-node/Dockerfile`         |
| Docker Compose   | `ryeos-node/docker-compose.yml` |
| Render blueprint | `ryeos-node/render.yaml`        |
| Local run script | `ryeos-node/run.sh`             |
| Server README    | `ryeos-node/README.md`          |

## Relationship to Other Docs

| Doc                                                     | Relationship                                                                                   |
| ------------------------------------------------------- | ---------------------------------------------------------------------------------------------- |
| [Execution Nodes](execution-nodes.md)                   | Architecture — routing tool, status cache, topology config. This doc is the operational guide. |
| [Remote Execution](remote-execution.md)                 | Protocol layer — CAS sync, materializer, trust model.                                          |
| [Sovereign Inference](../future/sovereign-inference.md) | GPU nodes and completions server come after this. This doc is the foundation.                  |
