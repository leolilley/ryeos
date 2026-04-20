# rye:signed:2026-04-20T05:46:18Z:ac2e0a1722bba014b7df18adb8628b8386639226a0632517573a4aee310cd6c1:iZIAWxwvst_5TvEV2Eo1KdLEJD4BwQdlUlBRlAV-r6WMrzX4vCXSasVhIs6qT8HAORil5Mpph1xY6OS9DUTqCQ:4b987fd4e40303ac
"""Thread state persistence — DELETED as runtime authority in v3.

The daemon (ryeosd) owns thread state, cancellation, and suspension.
Filesystem control files (.cancel_requested, .suspend_requested, state.json)
are no longer authoritative.

This module is retained only as a stub so existing imports fail loudly.
"""

__version__ = "2.0.0"
__tool_type__ = "python"
__category__ = "rye/agent/threads/persistence"
__tool_description__ = "DELETED — thread state persistence is daemon-owned in v3"


class StateStore:
    """Stub — raises on instantiation."""

    def __init__(self, *args, **kwargs):
        raise RuntimeError(
            "StateStore is deleted in v3; "
            "thread state and control is daemon-owned via ryeosd"
        )
