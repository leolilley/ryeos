# Build from project root: docker build -f services/registry-api/Dockerfile .
#
# The registry API is the server side. It only needs:
# - rye.utils.* and rye.constants (core Python code)
# - rye/core/parsers and rye/core/extractors (data bundle items for validation)
# - lillux primitives (signing, integrity)
#
# It does NOT need the full ryeos bundle (MCP tools, registry client,
# agent threads, etc.) — those are client-side concerns.
# Uses ryeos-bare which ships no data-driven tools.
FROM python:3.11-slim

WORKDIR /app

# Install system dependencies
RUN apt-get update && apt-get install -y --no-install-recommends \
    curl \
    && rm -rf /var/lib/apt/lists/*

# Copy lillux kernel
COPY lillux/kernel /app/lillux

# Copy ryeos-bare pyproject.toml and the rye module it packages.
# The symlink (ryeos-bare/rye -> ../ryeos/rye) doesn't work in Docker,
# so we copy the rye module directly into the ryeos-bare directory.
COPY ryeos-bare/pyproject.toml /app/ryeos-bare/pyproject.toml
COPY ryeos/rye /app/ryeos-bare/rye

# Copy registry-api
COPY services/registry-api/pyproject.toml services/registry-api/README.md /app/registry-api/
COPY services/registry-api/registry_api /app/registry-api/registry_api

# Install packages
WORKDIR /app/registry-api
RUN pip install --no-cache-dir /app/lillux/ /app/ryeos-bare/ -e .

# Expose port
EXPOSE 8000

# Health check — Railway handles this via healthcheckPath in railway.toml
HEALTHCHECK --interval=30s --timeout=10s --start-period=30s --retries=3 \
    CMD curl -f http://localhost:${PORT:-8000}/health || exit 1

# Run server — Railway injects PORT env var
CMD uvicorn registry_api.main:app --host 0.0.0.0 --port ${PORT:-8000}
