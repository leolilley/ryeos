FROM python:3.12-slim
WORKDIR /app
COPY pyproject.toml .
RUN pip install --no-cache-dir .
COPY ryeos_remote_mcp ryeos_remote_mcp
CMD uvicorn ryeos_remote_mcp.server:app --host 0.0.0.0 --port ${PORT:-8000}
