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

CMD ["sh", "-c", "python -m ryeos_node.init $CAS_BASE_PATH && uvicorn ryeos_node.server:app --host 0.0.0.0 --port ${PORT:-8000}"]
