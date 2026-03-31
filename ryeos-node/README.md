# ryeos-node

CAS-native execution node — accepts signed requests, executes directives and tools, syncs via CAS.

## Deploy

### Modal

```bash
cd ryeos-node
modal deploy modal_app.py
```

Volume at `/cas` persists signing key, node config, and per-user CAS storage.

### Render

Push to a repo connected to Render. The `render.yaml` blueprint creates a web service with a persistent disk at `/cas`.

### Docker

```bash
cd ryeos-node
docker compose up
```

### Local

```bash
cd ryeos-node
pip install ryeos "fastapi[standard]" pydantic-settings pyyaml
./run.sh
```

Defaults to `~/.ai/node` for CAS storage. Override with `CAS_BASE_PATH`.

## First Boot

All targets call `python -m ryeos_node.init` on startup, which:
1. Generates Ed25519 + X25519 signing keys
2. Creates `authorized_keys/` directory
3. Scaffolds `node.yaml` with defaults
4. Prints the node fingerprint

## Add Authorized Keys

```bash
# Get your fingerprint from your local machine
cat ~/.ai/signing/id_ed25519.pub

# Add to the node (replace <fp> with your fingerprint)
cat > <cas>/config/authorized_keys/<fp>.toml << 'EOF'
owner = "you"
capabilities = ["rye.*"]
EOF
```

## Configure Node

Edit `<cas>/config/.ai/config/node/node.yaml`:

```yaml
identity:
  name: my-node
hardware:
  gpus: 0
  memory_gb: 2
features:
  webhooks: true
limits:
  max_concurrent: 8
coordination:
  type: asyncio
```

## Endpoints

| Endpoint | Auth | Purpose |
|---|---|---|
| `GET /health` | None | Health check |
| `GET /status` | None | Node capabilities, load, hardware |
| `GET /public-key` | None | Node's Ed25519 public key (TOFU) |
| `POST /execute` | Signed | Execute a tool or directive |
| `POST /push` | Signed | Push project snapshot |
| `POST /objects/has` | Signed | CAS sync — check existence |
| `POST /objects/put` | Signed | CAS sync — upload objects |
| `POST /objects/get` | Signed | CAS sync — download objects |
