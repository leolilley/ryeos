<!-- ryeos:signed:2026-05-23T09:45:40Z:e02a43c5ec646cb61c6e04bb27d350688d578215889d244c09abe929c977858e:9rwMzoyHrri/QTtZuCjDTnstdmgRJR0w8rnCabLt/hqFQkfw6p8Bn5T7Ov2d2Aldp6GXWQqZMZ4XF/bRKgEDAA==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->
---
category: ryeos/standard/handlers
tags: [handler, graph, permissions]
version: "1.0.0"
description: Graph permissions composer reference.
---

# Handler: graph-permissions

Invariant: graph-permissions preserves graph configuration and lifts declared graph permissions into `policy_facts.effective_caps`.

Graph action callbacks use daemon-side capability tokens just like directive tool callbacks. Without this composer, graph callbacks would receive deny-all caps and fail at the UDS/dispatch boundary.
