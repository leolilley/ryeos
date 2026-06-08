<!-- ryeos:signed:2026-06-08T00:42:18Z:e78988a2e1738868ba4c8ee771c9bb45dbcab99be1b56682169203c6cff8af8f:I/2VHpLdQiA/Isp82nMZ1Oq3jHxsrQRjkatynv1KIokBC9ppKft1NXyEDAIl+sLHwz87OKVGtBLPCfLqa+pMAg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
# ryeos:signed:2026-06-07T05:37:38Z:95417768d40775d343015bb79fa0f1163e391134ca4b26dca583357afc3c1ba7:SSv1m5KFbBOHiHT8pQr1hwcCNOCzVu37joyhdShDfJOb6slr0ZeqS01JgaZkVyD7VEiAtPy/YiP27666CuMnDg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
---
category: ryeos/core/handlers
tags: [handler, parser, regex]
version: "1.0.0"
description: Regex key-value parser handler reference.
---

# Handler: regex-kv

Invariant: `regex-kv` extracts named metadata fields from source text using configured regular expressions.

It backs lightweight source parsers such as JavaScript constants. Parser descriptors provide the patterns, key normalization, and output schema; the handler only performs extraction and returns a mapping.
