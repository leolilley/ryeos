# Registry Deployment Guide

Complete guide for deploying the RYE Registry infrastructure.

## Architecture Overview

```
┌───────────────────────────────────────────────────────────────────────────┐
│                                CLIENTS                                    │
│  (rye CLI, MCP servers, integrations)                                     │
└───────────────────────────────────────────────────────────────────────────┘
                                    │
                                    │ HTTPS
                                    ▼
┌───────────────────────────────────────────────────────────────────────────┐
│                           REGISTRY API                                    │
│  FastAPI service (Python)                                                 │
│  - POST /v1/push    → Validate + sign + store                             │
│  - GET  /v1/pull    → Fetch + return signed content                       │
│  - GET  /v1/search  → Search items                                        │
│  - GET  /health     → Health check                                        │
└───────────────────────────────────────────────────────────────────────────┘
                                    │
                                    │ Service Role Key (bypasses RLS)
                                    ▼
┌───────────────────────────────────────────────────────────────────────────┐
│                            SUPABASE                                       │
│  - PostgreSQL database (with RLS enabled)                                 │
│  - Auth (JWT tokens)                                                      │
│  - Storage (optional, for large files)                                    │
└───────────────────────────────────────────────────────────────────────────┘
```

## Prerequisites

- Supabase project (or self-hosted Supabase)
- Python 3.11+
- Docker (for production deployment)
- Domain with SSL certificate (for production)

---

## Part 1: Supabase Setup

### 1.1 Create Supabase Project

1. Go to [supabase.com](https://supabase.com) and create a new project
2. Note your project details:
   - **Project URL**: `https://<project-id>.supabase.co`
   - **Anon Key**: Found in Settings → API → Project API keys
   - **Service Role Key**: Found in Settings → API → Project API keys (keep secret!)
   - **JWT Secret**: Found in Settings → API → JWT Settings

### 1.2 Apply Database Schema

Run the migrations in order:

```bash
# Connect to your Supabase SQL editor or use psql

# 1. Initial schema
psql $DATABASE_URL < docs/db/schema/001_initial_schema.sql

# 2. Helper functions (if any)
psql $DATABASE_URL < docs/db/schema/002_functions.sql

# 3. RLS policies (locks down direct access)
psql $DATABASE_URL < docs/db/schema/003_rls_api_only.sql
```

Or via Supabase Dashboard:

1. Go to SQL Editor
2. Paste and run each migration file in order

### 1.3 Verify RLS is Enabled

```sql
-- Check RLS status
SELECT tablename, rowsecurity
FROM pg_tables
WHERE schemaname = 'public';
```

All tables should show `rowsecurity = true`.

---

## Part 2: Registry API Deployment

### 2.1 Environment Configuration

Create `.env` file from template:

```bash
cd services/registry-api
cp .env.example .env
```

Edit `.env` with your Supabase credentials:

```env
# Supabase - Get from: Dashboard → Settings → API
SUPABASE_URL=https://your-project-id.supabase.co

# Secret key (Settings → API → Secret keys → New secret key)
# Format: sb_secret_xxx - bypasses RLS for backend operations
SUPABASE_SERVICE_KEY=sb_secret_xxx

# JWT Secret (Settings → API → JWT Settings → JWT Secret)
SUPABASE_JWT_SECRET=your-jwt-secret

# Server
HOST=0.0.0.0
PORT=8000
LOG_LEVEL=INFO

# CORS (set your allowed origins)
ALLOWED_ORIGINS=https://your-app.com,https://localhost:3000
```

### 2.2 Production Deployment (Docker)

Build from project root (required to include rye/lilux packages):

```bash
# From project root (rye-os/)
cd /path/to/rye-os

# Build image
docker build -f services/registry-api/Dockerfile -t registry-api:latest .

# Run container
docker run -d \
  --name registry-api \
  -p 8000:8000 \
  -e SUPABASE_URL=$SUPABASE_URL \
  -e SUPABASE_SERVICE_KEY=$SUPABASE_SERVICE_KEY \
  -e SUPABASE_JWT_SECRET=$SUPABASE_JWT_SECRET \
  -e LOG_LEVEL=INFO \
  --restart unless-stopped \
  registry-api:latest
```

### 2.3 Production Deployment (Railway/Fly.io)

**Railway:**

The project includes a `railway.toml` that configures the build from project root.

```bash
# Install Railway CLI
npm install -g @railway/cli

# Login and link (from project root)
railway login
railway link

# Set environment variables
railway variables set SUPABASE_URL=https://xxx.supabase.co
railway variables set SUPABASE_SERVICE_KEY=sb_secret_xxx
railway variables set SUPABASE_JWT_SECRET=your-jwt-secret

# Deploy
railway up
```

**Fly.io:**

```bash
# Install flyctl
curl -L https://fly.io/install.sh | sh

# Launch app (from project root)
fly launch --dockerfile services/registry-api/Dockerfile

# Set secrets
fly secrets set SUPABASE_URL=https://xxx.supabase.co
fly secrets set SUPABASE_SERVICE_KEY=sb_secret_xxx
fly secrets set SUPABASE_JWT_SECRET=your-jwt-secret

# Deploy
fly deploy
```

### 2.4 Verify Deployment

```bash
# Health check
curl https://your-api-domain.com/health

# Expected response:
# {"status":"healthy","version":"0.1.0","database":"connected"}
```

---

## Part 3: Client Configuration

### 3.1 Configure Registry URL

Set the registry URL for `rye` clients:

```bash
# Environment variable
export RYE_REGISTRY_URL=https://your-api-domain.com

# Or in .env file
echo "RYE_REGISTRY_URL=https://your-api-domain.com" >> ~/.config/rye/.env
```

### 3.2 Authenticate

Via MCP execute tool:

```json
{
  "item_type": "tool",
  "item_id": "registry",
  "project_path": "/path/to/project",
  "parameters": {
    "action": "login"
  }
}
```

Verify authentication:

```json
{
  "item_type": "tool",
  "item_id": "registry",
  "project_path": "/path/to/project",
  "parameters": {
    "action": "whoami"
  }
}
```

### 3.3 Test Operations

**Search:**

```json
{
  "item_type": "tool",
  "item_id": "registry",
  "project_path": "/path/to/project",
  "parameters": {
    "action": "search",
    "query": "bootstrap",
    "item_type": "directive"
  }
}
```

**Pull:**

```json
{
  "item_type": "tool",
  "item_id": "registry",
  "project_path": "/path/to/project",
  "parameters": {
    "action": "pull",
    "item_type": "directive",
    "item_id": "core/bootstrap"
  }
}
```

**Push:**

```json
{
  "item_type": "tool",
  "item_id": "registry",
  "project_path": "/path/to/project",
  "parameters": {
    "action": "push",
    "item_type": "directive",
    "item_path": ".ai/directives/my-directive.md",
    "name": "me/my-directive",
    "version": "1.0.0"
  }
}
```

---

## Part 4: Local Development Setup

### 4.1 Prerequisites

```bash
# Install Python 3.11+
python --version  # Should be 3.11+

# Clone repository
git clone https://github.com/your-org/rye-os.git
cd rye-os
```

### 4.2 Install Dependencies

```bash
# Create virtual environment
python -m venv .venv
source .venv/bin/activate  # Linux/macOS
# or: .venv\Scripts\activate  # Windows

# Install lilux first (rye dependency)
pip install -e lilux/

# Install rye package (for validators)
pip install -e rye/

# Install registry-api
pip install -e services/registry-api/
```

### 4.3 Local Supabase (Optional)

For fully local development, use Supabase CLI:

```bash
# Install Supabase CLI
npm install -g supabase

# Initialize
supabase init

# Start local Supabase
supabase start

# Apply migrations
supabase db push

# Get local credentials
supabase status
```

Local credentials:

- **URL**: `http://localhost:54321`
- **Service Key**: (shown in `supabase status`)
- **JWT Secret**: (shown in `supabase status`)

### 4.4 Run Registry API Locally

```bash
cd services/registry-api

# Create .env with local or test Supabase credentials
cat > .env << EOF
SUPABASE_URL=http://localhost:54321
SUPABASE_SERVICE_KEY=your-local-service-key
SUPABASE_JWT_SECRET=your-local-jwt-secret
LOG_LEVEL=DEBUG
EOF

# Run with auto-reload
uvicorn registry_api.main:app --reload --port 8000
```

### 4.5 Test Against Local API

```bash
# Set registry URL to local
export RYE_REGISTRY_URL=http://localhost:8000

# Run tests
cd services/registry-api
pytest -v

# Manual testing
curl http://localhost:8000/health
```

### 4.6 Run Full Test Suite

```bash
# From project root
cd rye-os

# Run all tests
pytest

# Run registry-api tests only
pytest services/registry-api/

# Run with coverage
pytest --cov=registry_api services/registry-api/
```

---

## Part 5: Security Checklist

### 5.1 Supabase Security

- [ ] RLS enabled on all tables (migration 003)
- [ ] Service role key is NOT exposed to clients
- [ ] Anon key has limited permissions (read-only for public data)
- [ ] JWT secret is secure and not shared

### 5.2 Registry API Security

- [ ] HTTPS enabled in production
- [ ] CORS configured for allowed origins only
- [ ] Rate limiting configured (optional, can use Cloudflare/nginx)
- [ ] Logs do not contain secrets

### 5.3 Client Security

- [ ] Tokens stored securely (keyring, not plaintext)
- [ ] Environment variables used for sensitive config
- [ ] No tokens committed to git

---

## Part 6: Monitoring & Maintenance

### 6.1 Health Monitoring

Set up monitoring for:

- `/health` endpoint
- Database connection status
- API response times

### 6.2 Log Aggregation

Configure log shipping to your preferred service:

- Datadog
- Grafana Loki
- AWS CloudWatch

### 6.3 Backup Strategy

Supabase automatically backs up data. For self-hosted:

```bash
# Daily backup
pg_dump $DATABASE_URL > backup-$(date +%Y%m%d).sql
```

---

## Troubleshooting

### API returns 401 Unauthorized

- Check JWT token is valid and not expired
- Verify `SUPABASE_JWT_SECRET` matches your project
- Ensure user has logged in: `rye registry login`

### API returns 403 Forbidden

- RLS policies may be blocking access
- Check if using service role key (bypasses RLS)
- Verify table policies with: `SELECT * FROM pg_policies`

### Database connection errors

- Check `SUPABASE_URL` is correct
- Verify `SUPABASE_SERVICE_KEY` has access
- Check network connectivity to Supabase

### Validation errors on push

- Run `rye sign` locally first to see validation issues
- Check extractor schemas match expected format
- Ensure all required fields are present

---

## Quick Reference

### Environment Variables

| Variable               | Description                     | Required |
| ---------------------- | ------------------------------- | -------- |
| `SUPABASE_URL`         | Supabase project URL            | Yes      |
| `SUPABASE_SERVICE_KEY` | Service role key                | Yes      |
| `SUPABASE_JWT_SECRET`  | JWT secret for token validation | Yes      |
| `RYE_REGISTRY_URL`     | Registry API URL (client-side)  | Yes      |
| `RYE_REGISTRY_TOKEN`   | Auth token (CI/headless)        | No       |

### API Endpoints

| Method | Endpoint               | Auth | Description  |
| ------ | ---------------------- | ---- | ------------ |
| GET    | `/health`              | No   | Health check |
| GET    | `/v1/search`           | No   | Search items |
| GET    | `/v1/pull/{type}/{id}` | No   | Pull item    |
| POST   | `/v1/push`             | Yes  | Push item    |

### MCP Tool Usage

All registry operations go through `mcp__rye__execute`:

```json
{
  "item_type": "tool",
  "item_id": "registry",
  "project_path": "/path/to/project",
  "parameters": {
    "action": "<action>",
    ...
  }
}
```

**Available actions:**

- `login` - Start device auth flow
- `logout` - Clear auth session
- `whoami` - Show current user
- `search` - Search registry items
- `pull` - Download item from registry
- `push` - Upload item to registry
- `set_visibility` - Change item visibility
