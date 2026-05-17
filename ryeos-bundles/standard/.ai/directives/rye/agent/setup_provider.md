<!-- ryeos:signed:2026-05-17T21:44:36Z:0c8cb87426436b9c80e41eca307da924588394ef644f8e7f29305ecffc5c0f0a:xR92+I1gnSZlH8F7hG9uVgad7oiq6ZNTD8RnlXuLAw3Gn1PaUDMEyL03R1ESXcC0Bn6e89n85P9VjNVTMP4GCQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
description: "Configure an LLM provider by writing/updating the provider YAML config with API key env var reference and default model."
version: "1.0.0"
model_tier: fast
limits:
  turns: 6
  tokens: 4096
permissions:
  execute:
    - tool:rye.file-system.*
  fetch:
    - tool:*
---

# Setup Provider

Configure an LLM provider (Anthropic or OpenAI) by writing/updating the provider YAML config.

<process>
  <step name="check_existing">
    Check if provider YAML already exists at `.ai/tools/rye/agent/providers/{input:provider}.yaml`.
    If it exists, load current config to preserve any existing settings.
  </step>

  <step name="load_system_default">
    Load the system default config for the provider:
    `rye_fetch(item_type="tool", item_id="rye/agent/providers/{input:provider}")`
    Use this as the base template for the config.
  </step>

  <step name="write_config">
    Write updated config to user space `~/.ai/tools/rye/agent/providers/{input:provider}.yaml` with:
    - The API key env var reference set to `${input:api_key_env_var}`
    - The default model set to {input:default_model} if provided, otherwise keep the system default
    - All other settings preserved from the system default

    `rye_execute(item_id="rye/file-system/write", parameters={"path": "~/.ai/tools/rye/agent/providers/{input:provider}.yaml", "content": "<generated YAML config>", "create_dirs": true})`

  </step>

  <step name="verify_provider">
    Verify the provider resolves correctly by loading the config back and confirming the env var reference and model are set.
    `rye_fetch(item_type="tool", item_id="rye/agent/providers/{input:provider}", source="user")`
  </step>
</process>

<success_criteria>
<criterion>Provider config written to ~/.ai/tools/rye/agent/providers/{input:provider}.yaml</criterion>
<criterion>API key env var reference correctly set</criterion>
<criterion>Default model configured if provided</criterion>
<criterion>Provider resolves correctly after setup</criterion>
</success_criteria>
