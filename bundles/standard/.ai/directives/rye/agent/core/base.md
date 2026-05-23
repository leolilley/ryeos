<!-- ryeos:signed:2026-05-23T09:45:41Z:cd66123b1fb225405153d2ad4dba2ee0d46a2d19265886abb19ce50f83164bdb:ucUiuaNih2XJ88qyn8nXGYHI+e50ZuH+6DxOx8zOxiV6JMDJUEyygPnIIVLaQ8y+moSQ5mxgoVu/iAbXrzxwAA==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->
---
description: "Rye agent base — extends general agent base with Rye identity and behavior"
version: "2.1.0"
extends: directive:agent/core/base
context:
  system:
    - knowledge:rye/agent/core/Identity
    - knowledge:rye/agent/core/Behavior
  suppress:
    - knowledge:agent/core/Behavior
permissions:
  execute:
    - "*"
  fetch:
    - "*"
  sign:
    - "*"
---

Standard operating context for Rye agent threads.
