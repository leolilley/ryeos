# Build from project root: docker build -f services/ryeos-remote-mcp/Dockerfile .
#
# Stateless MCP-over-HTTP proxy for ryeos-remote on Modal.
# No engine, no CAS — just httpx + mcp SDK + starlette.
FROM python:3.12-slim

WORKDIR /app

# Copy service files
COPY services/ryeos-remote-mcp/pyproject.toml /app/
COPY services/ryeos-remote-mcp/ryeos_remote_mcp /app/ryeos_remote_mcp

# Install dependencies
RUN pip install --no-cache-dir .

EXPOSE 8000

CMD uvicorn ryeos_remote_mcp.server:app --host 0.0.0.0 --port ${PORT:-8000}
