# rye:signed:2026-02-26T05:02:30Z:b0b295fadfbf40e9e679078af1d71e6fb605e720838914d8f4a8da1fde0cc2ad:RqPnvdPU3_nWCyhZE7Lv9yrLad6a_oNEc_S_c9CUqGPgj53lD11rd0wRAaAbuYRN4sHuCzuejbvA80H0JwPaBw==:4b987fd4e40303ac
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
