#!/usr/bin/env python3
"""Pre-RyeOS public-registry transport boundary.

Seeds the exact public index records and opaque locked crate archives needed
before import/binding and offline Cargo. Selection, checksum validation and
local-registry assembly have one implementation beside the signed Tool.
This entry deliberately requires host Python/PyYAML, curl >= 8.4, TLS/resolver
configuration and network access; none is inherited by an admitted Tool.
"""
from __future__ import annotations

import argparse
from pathlib import Path
import re
import subprocess
import sys
import time

import yaml

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / ".ai/tools/ryeos/development/registry-production/lib"))
from registry_inputs import assemble, bounded_file, validate_config, validate_url

class Fetcher:
    def __init__(self, config):
        self.config = config
        self.total = 0
        self.deadline = time.monotonic() + config["limits"]["total_timeout_seconds"]
        self.checked_curl = False

    def __call__(self, url, maximum):
        validate_url(url, self.config["allowed_https_hosts"])
        remaining = self.deadline - time.monotonic()
        if remaining <= 0:
            raise ValueError("acquisition lifetime exhausted")
        limit = self.config["limits"]
        if not self.checked_curl:
            version = subprocess.run(["curl", "--disable", "--version"],
                                     stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                     timeout=remaining, check=True).stdout
            match = re.match(rb"curl (\d+)\.(\d+)\.(\d+) ", version)
            # Since 8.4, max-filesize also stops transfers without a declared
            # Content-Length. This is a required implementation capability,
            # not an optional behavior or a replacement for the byte budget.
            if match is None or tuple(map(int, match.groups())) < (8, 4, 0):
                raise ValueError("bootstrap acquisition requires curl >= 8.4.0")
            self.checked_curl = True
            remaining = self.deadline - time.monotonic()
            if remaining <= 0:
                raise ValueError("acquisition lifetime exhausted")
        maximum = min(maximum, limit["max_total_download_bytes"] - self.total)
        if maximum <= 0:
            raise ValueError("acquisition byte bound exhausted")
        request_remaining = min(remaining, limit["request_timeout_seconds"])
        # This explicit bootstrap uses the same host download dependency as
        # Stage-0 acquisition, never host Cargo. Curl disables user config and
        # proxies, accepts HTTPS only and does not follow redirects. Its byte
        # limit bounds captured output; the independent process deadline also
        # covers slow-drip reads and resolver/connect stalls. This is not a
        # worker transport or a new RyeOS process owner.
        result = subprocess.run([
            "curl", "--disable", "--silent", "--show-error", "--fail",
            "--proto", "=https", "--noproxy", "*", "--max-redirs", "0",
            "--max-filesize", str(maximum), "--max-time", str(request_remaining),
            "--connect-timeout", str(request_remaining),
            "--url", url,
        ], stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=request_remaining, check=True)
        data = result.stdout
        self.total += len(data)
        if len(data) > maximum or time.monotonic() > self.deadline:
            raise ValueError("acquisition byte or lifetime bound exceeded")
        return data


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--lock", type=Path, required=True)
    parser.add_argument("--declaration", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    config = yaml.safe_load(bounded_file(args.declaration, 65536))
    validate_config(config)
    lock = bounded_file(args.lock, config["limits"]["max_input_file_bytes"])
    assemble(lock, config, args.output, Fetcher(config), source_kind="public_https_acquisition")
    print(f"Acquired locked registry inputs: {args.output}; import/binding and admitted Cargo production remain required")


if __name__ == "__main__":
    main()
