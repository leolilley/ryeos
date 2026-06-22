<!-- ryeos:signed:2026-06-22T04:23:11Z:ed2a9ef32f22240cccd7a05699f13bc63145acbc02588ee7acf6380f9bb718bd:axXi+3nQGLHSqTW2dWB5RaqCgskntyqzwW/G2y3ZpyU0gGuuGaNpqhY+sACc0kFcZtkWTCwkvKDPgajPiGB5Bg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard/handlers
tags: [handler, graph, permissions]
version: "1.0.0"
description: Graph permissions composer reference.
---

# Handler: graph-permissions

Invariant: graph-permissions preserves graph configuration and lifts `requires.capabilities.declared` into `policy_facts.effective_caps`; it rejects a legacy top-level `permissions:` block and a malformed `requires` tree at compose time.

Graph action callbacks use daemon-side capability tokens just like directive tool callbacks. Without this composer, graph callbacks would receive deny-all caps and fail at the UDS/dispatch boundary.
