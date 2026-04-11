"""Legacy detached child entrypoint.

Detached execution is disabled on the v3 daemon-owned path because the old
async runner still depended on local registry authority. If this module is
invoked directly, it returns a structured error instead of mutating local
lifecycle state.
"""

import json
import logging
import os
import sys

logger = logging.getLogger(__name__)


def main():
    import argparse

    parser = argparse.ArgumentParser(description="Async execution runner")
    parser.add_argument("--project-path", required=True)
    parser.add_argument("--thread-id", required=True)
    args = parser.parse_args()

    if os.environ.get("RYE_DEBUG"):
        logging.basicConfig(
            level=logging.DEBUG,
            format="[async_runner] %(levelname)s: %(message)s",
            stream=sys.stderr,
        )

    # Read stdin to avoid broken-pipe surprises for callers that still send a payload.
    _ = sys.stdin.read()
    result = {
        "status": "error",
        "thread_id": args.thread_id,
        "error": (
            "rye.utils.async_runner is disabled on the daemon-owned v3 path; "
            "detached execution must be recreated as a daemon client"
        ),
    }
    print(json.dumps(result, default=str))
    sys.exit(1)


if __name__ == "__main__":
    main()
