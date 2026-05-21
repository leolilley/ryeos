<!-- ryeos:signed:2026-05-21T11:11:49Z:e02a43c5ec646cb61c6e04bb27d350688d578215889d244c09abe929c977858e:VMoOya0V57hdcfN2JDReB5HsLX7qdSIrzNKoC+cLdklliSwSxCIbuWW8KXFPZEUlJrO6FwoqDIIil52VGr+YBA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard/handlers
tags: [handler, graph, permissions]
version: "1.0.0"
description: Graph permissions composer reference.
---

# Handler: graph-permissions

Invariant: graph-permissions preserves graph configuration and lifts declared graph permissions into `policy_facts.effective_caps`.

Graph action callbacks use daemon-side capability tokens just like directive tool callbacks. Without this composer, graph callbacks would receive deny-all caps and fail at the UDS/dispatch boundary.
