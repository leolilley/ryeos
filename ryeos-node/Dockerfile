FROM python:3.12-slim

WORKDIR /app

COPY requirements.txt /app/requirements.txt
RUN pip install --no-cache-dir -r requirements.txt

COPY ryeos_node/ /app/ryeos_node/

ENV CAS_BASE_PATH=/cas \
    PYTHONPATH=/app \
    RYE_KERNEL_PYTHON=/usr/local/bin/python3

EXPOSE 8000

CMD ["sh", "-c", "python -m ryeos_node.init $CAS_BASE_PATH && uvicorn ryeos_node.server:app --host 0.0.0.0 --port ${PORT:-8000}"]
