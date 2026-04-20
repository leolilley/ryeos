<!-- rye:signed:2026-04-19T09:49:53Z:5954c25bcf6235e44bf94ecdace11f6eebb6e08ab8b614ce436213cd37002c69:5o9Ei8fsguM8VZPvw6jT76LkUhMnzNxSZtcLa9Cmd7yNMSV7xl3lCbJiw2R+0f+/JclX2fARKcjo2UJRFlgcCg==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb
# Thread Directive

Execute a directive in a managed thread with an LLM loop.

```xml
<directive name="thread_directive" version="1.0.0">
  <metadata>
    <description>Execute a directive in a managed thread with an LLM loop. Main user-facing directive for thread execution.</description>
    <category>rye/agent/threads</category>
    <author>rye-os</author>
    <model tier="general" />
    <limits turns="4" tokens="4096" />
    <permissions>
      <execute>
        <tool>rye.agent.threads.thread_directive</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="directive_id" type="string" required="true">
      Fully qualified directive ID to execute (e.g., "rye/core/create_directive")
    </input>
    <input name="async" type="boolean" required="false">
      Run asynchronously (default: false). When true, returns immediately with thread_id.
    </input>
    <input name="inputs" type="object" required="false">
      Input parameters to pass to the directive
    </input>
    <input name="model" type="string" required="false">
      Override the model used for thread execution
    </input>
    <input name="limit_overrides" type="object" required="false">
      Override default limits (turns, tokens, spend)
    </input>
  </inputs>

  <outputs>
    <output name="thread_id">Unique identifier for the thread</output>
    <output name="status">Thread completion status (completed, running, failed)</output>
    <output name="cost">Total cost of the thread execution</output>
    <output name="result">Result text from the directive execution</output>
  </outputs>
</directive>
```

<process>
  <step name="validate_input">
    Validate that {input:directive_id} is non-empty and well-formed.
    If empty, halt with an error.
  </step>

  <step name="execute_thread">
    Execute the directive in a managed thread:
    `rye_execute(item_id="rye/agent/threads/thread_directive", parameters={"directive_id": "{input:directive_id}", "async": {input:async}, "inputs": {input:inputs}, "model": "{input:model}", "limit_overrides": {input:limit_overrides}})`
  </step>

  <step name="return_result">
    Return the thread result containing thread_id, status, cost, and result text.
    If async was true, return immediately with the thread_id and status "running".
  </step>
</process>

<success_criteria>
  <criterion>Directive name is validated as non-empty</criterion>
  <criterion>Thread execution tool called with correct parameters</criterion>
  <criterion>Result includes thread_id, status, cost, and result text</criterion>
</success_criteria>
