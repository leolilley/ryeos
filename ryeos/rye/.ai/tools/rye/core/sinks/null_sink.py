# rye:signed:2026-02-22T09:00:56Z:b0b295fadfbf40e9e679078af1d71e6fb605e720838914d8f4a8da1fde0cc2ad:rD0e5c06QRawLrL9nPQYMZZT04GqizPOkPqQU2yAwTRow40bM2YFAQs_c-t-te-jIzEAV0edyBDcHEu8ec5vAQ==:9fbfabe975fa5a7f
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
