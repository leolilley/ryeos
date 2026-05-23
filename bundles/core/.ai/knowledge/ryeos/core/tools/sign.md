<!-- ryeos:signed:2026-05-23T07:18:21Z:33303c15a5c6d86d8eaf5b771e80d18be8c2cfb1e4c2d0c21638ee6675f0bb8e:sCMbB7db41SY64Ndn7cFoFe12ZHa3bKHPQbVD9JxRqQEYW8Pn5U8cT4WsYHmNaT/VR62xPZAfDUhr24dWJxCBQ==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->
---
category: ryeos/core/tools
tags: [tool, sign, signatures, offline]
version: "1.1.0"
description: Core sign tool and service reference.
---

# Tool and Service: sign

Invariant: the sign tool signs project/user items with the operator key; system bundle items are signed by publishers during bundle publish.

Availability: **offline**. The CLI runs `sign` in-process. No daemon is required.

```bash
ryeos sign <canonical-ref> --project <dir>
ryeos sign <canonical-ref> --project <dir> --source project
ryeos sign "tool:ryeos/core/*" --project <dir>
```

It calls `ryeos-core-tools sign` and supports the same canonical-ref and glob semantics as the CLI. The `--source` flag accepts `project` (default) or `user`. System source is rejected — bundle items are signed by publishers.

Sign is both a tool (subprocess, `tool:ryeos/core/sign`) and a service (`service:sign`). The CLI dispatches it as an offline service descriptor.
