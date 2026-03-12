FROM python:3.12-slim
WORKDIR /app
COPY pyproject.toml .
RUN pip install .
COPY . .
CMD uvicorn ryeos_remote_mcp.server:app --host 0.0.0.0 --port ${PORT:-8000}
