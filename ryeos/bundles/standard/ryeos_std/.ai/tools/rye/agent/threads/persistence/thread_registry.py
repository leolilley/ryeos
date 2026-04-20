# rye:signed:2026-04-19T09:49:53Z:89244afbce1f4e07159091cfafb57e879f9c858b72e7382a23467d84c354777d:3CZiB5zVyX/fAApSyd/P9RTokqdVBSqbHenbyNaffkt6DBxJOGwLD43QBlWwBs5629P+MacABI16WqHjdmN4Cw==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb
"""Thread registry — DELETED as runtime authority in v3.

The daemon (ryeosd) is the sole authority for thread registration,
status updates, result storage, continuation links, and child discovery.

This module is retained only as a stub so existing imports fail loudly
instead of silently importing stale code.
"""

__version__ = "2.0.0"
__tool_type__ = "python"
__category__ = "rye/agent/threads/persistence"
__tool_description__ = "DELETED — thread registry is daemon-owned in v3"


class ThreadRegistry:
    """Stub — raises on instantiation."""

    def __init__(self, *args, **kwargs):
        raise RuntimeError(
            "ThreadRegistry is deleted in v3; "
            "thread lifecycle is daemon-owned via ryeosd"
        )


def get_registry(*args, **kwargs):
    raise RuntimeError(
        "get_registry() is deleted in v3; "
        "thread lifecycle is daemon-owned via ryeosd"
    )
