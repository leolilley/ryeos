# Build from project root: docker build -f services/registry-api/Dockerfile .
FROM python:3.11-slim

WORKDIR /app

# Install system dependencies
RUN apt-get update && apt-get install -y --no-install-recommends \
    git \
    && rm -rf /var/lib/apt/lists/*

# Copy lilux package (rye dependency)
COPY lilux /app/lilux

# Copy rye package
COPY rye /app/rye

# Copy registry-api
COPY services/registry-api/pyproject.toml services/registry-api/README.md /app/registry-api/
COPY services/registry-api/registry_api /app/registry-api/registry_api

# Install packages in order
WORKDIR /app/registry-api
RUN pip install --no-cache-dir /app/lilux/ /app/rye/ -e .

# Expose port
EXPOSE 8000

# Health check
HEALTHCHECK --interval=30s --timeout=10s --start-period=5s --retries=3 \
    CMD python -c "import httpx; httpx.get('http://localhost:8000/health').raise_for_status()"

# Run server
CMD ["uvicorn", "registry_api.main:app", "--host", "0.0.0.0", "--port", "8000"]
