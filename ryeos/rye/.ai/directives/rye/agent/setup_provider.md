<!-- rye:signed:2026-02-22T02:31:19Z:8e9647fcd231f3aa04428a71275a68457bed4327da0c652843a7f5c68d68a822:r6PSJuqSIZgtyO6eaHmYtmlxeUN9KgMT58RHPLrAsFZtldaIgioYqsjEdFB8c41eUjnX9dc0UCiW5BTJ2YaSBw==:9fbfabe975fa5a7f -->

# Setup Provider

Configure an LLM provider (Anthropic or OpenAI) by writing/updating the provider YAML config.

```xml
<directive name="setup_provider" version="1.0.0">
  <metadata>
    <description>Configure an LLM provider by writing/updating the provider YAML config with API key env var reference and default model.</description>
    <category>rye/agent</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits max_turns="6" max_tokens="4096" />
    <permissions>
      <execute>
        <tool>rye.file-system.*</tool>
      </execute>
      <search>
        <tool>*</tool>
      </search>
    </permissions>
  </metadata>

  <inputs>
    <input name="provider" type="string" required="true">
      LLM provider to configure (enum: anthropic, openai)
    </input>
    <input name="api_key_env_var" type="string" required="true">
      Name of the environment variable holding the API key (e.g., "ANTHROPIC_API_KEY")
    </input>
    <input name="default_model" type="string" required="false">
      Default model identifier to use for this provider
    </input>
  </inputs>

  <outputs>
    <output name="config_path">Path to the written provider YAML config</output>
    <output name="verified">Whether the provider resolves correctly after setup</output>
  </outputs>
</directive>
```

<process>
  <step name="check_existing">
    Check if provider YAML already exists at `.ai/tools/rye/agent/providers/{input:provider}.yaml`.
    If it exists, load current config to preserve any existing settings.
  </step>

  <step name="load_system_default">
    Load the system default config for the provider:
    `rye_load(item_type="tool", item_id="rye/agent/providers/{input:provider}")`
    Use this as the base template for the config.
  </step>

  <step name="write_config">
    Write updated config to user space `~/.ai/tools/rye/agent/providers/{input:provider}.yaml` with:
    - The API key env var reference set to `${input:api_key_env_var}`
    - The default model set to {input:default_model} if provided, otherwise keep the system default
    - All other settings preserved from the system default

    `rye_execute(item_type="tool", item_id="rye/file-system/write", parameters={"path": "~/.ai/tools/rye/agent/providers/{input:provider}.yaml", "content": "<generated YAML config>", "create_dirs": true})`

  </step>

  <step name="verify_provider">
    Verify the provider resolves correctly by loading the config back and confirming the env var reference and model are set.
    `rye_load(item_type="tool", item_id="rye/agent/providers/{input:provider}", source="user")`
  </step>
</process>

<success_criteria>
<criterion>Provider config written to ~/.ai/tools/rye/agent/providers/{input:provider}.yaml</criterion>
<criterion>API key env var reference correctly set</criterion>
<criterion>Default model configured if provided</criterion>
<criterion>Provider resolves correctly after setup</criterion>
</success_criteria>
