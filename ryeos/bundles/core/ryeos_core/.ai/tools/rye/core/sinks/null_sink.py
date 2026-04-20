# rye:signed:2026-04-19T09:49:53Z:b0b295fadfbf40e9e679078af1d71e6fb605e720838914d8f4a8da1fde0cc2ad:RqPnvdPU3/nWCyhZE7Lv9yrLad6a/oNEc/S/c9CUqGPgj53lD11rd0wRAaAbuYRN4sHuCzuejbvA80H0JwPaBw==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb
__tool_type__ = "runtime"
__version__ = "1.0.0"
__executor_id__ = "python"
__category__ = "rye/core/sinks"
__tool_description__ = "Null sink - discards all events without processing"


class NullSink:
    """Discard all events."""

    async def write(self, event: str) -> None:
        """Discard event."""
        pass

    async def close(self) -> None:
        """No-op close."""
        pass
