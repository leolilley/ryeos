# The registry API is the server side. It needs:
# - ryeos-core (engine + parsers/extractors for validation)
# - lillux primitives (signing, integrity)
FROM python:3.11-slim

WORKDIR /app

# Install system dependencies
RUN apt-get update && apt-get install -y --no-install-recommends \
    curl \
    && rm -rf /var/lib/apt/lists/*

# Copy registry-api
COPY pyproject.toml README.md /app/
COPY registry_api /app/registry_api

# Install packages — ryeos-core (includes engine + parsers/extractors) and lillux from PyPI
RUN pip install --no-cache-dir ryeos-core -e .

# Expose port
EXPOSE 8000

# Health check — Railway handles this via healthcheckPath in railway.toml
HEALTHCHECK --interval=30s --timeout=10s --start-period=30s --retries=3 \
    CMD curl -f http://localhost:${PORT:-8000}/health || exit 1

# Run server — Railway injects PORT env var
CMD uvicorn registry_api.main:app --host 0.0.0.0 --port ${PORT:-8000}
