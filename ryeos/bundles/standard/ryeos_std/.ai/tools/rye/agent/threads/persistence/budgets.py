# rye:signed:2026-04-19T09:49:53Z:69db493bdd6a6f7068d540d44bf40b47d49f7dc273b1d78353c8cba5bbfb9b1c:09dByfwRawAM3uEvlRsNfqC6rodc57fHwEX9k4Pz94HI2/VrKiYOOnQxZqLDfSXW9A5GRoEKl5/WbmGmmdoHCA==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb
"""Budget ledger — DELETED as runtime authority in v3.

The daemon (ryeosd) is the sole authority for budget reservation,
spend reporting, and release. The Python budget ledger is no longer
authoritative.

This module is retained only as a stub so existing imports fail loudly.
"""

__version__ = "2.0.0"
__tool_type__ = "python"
__category__ = "rye/agent/threads/persistence"
__tool_description__ = "DELETED — budget ledger is daemon-owned in v3"


class BudgetLedger:
    """Stub — raises on instantiation."""

    def __init__(self, *args, **kwargs):
        raise RuntimeError(
            "BudgetLedger is deleted in v3; "
            "budgets are daemon-owned via ryeosd"
        )


def get_ledger(*args, **kwargs):
    raise RuntimeError(
        "get_ledger() is deleted in v3; "
        "budgets are daemon-owned via ryeosd"
    )
