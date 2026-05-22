<!-- ryeos:signed:2026-05-22T03:35:36Z:0694ca8adcec29d3555bc7b33dc2f2681263bed25300d0f23407437b4e51e936:TFi4ZoX5S6utCoWpf+GMKT4Yk5qVwfCtSaoGi8DDJtgW+ULHmwjPL1y1JgwDibV1jqgjw6C3A9eyOMaig2RDDQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/services
tags: [service, signing, node]
version: "1.0.0"
description: Node-sign service reference.
---

# Service: node-sign

Invariant: node-sign signs daemon-owned node records with the node identity, not arbitrary bundle items.

Bundle items are signed by bundle publishers during publish. Node-sign is for daemon-internal state records whose trust root is the node identity.
