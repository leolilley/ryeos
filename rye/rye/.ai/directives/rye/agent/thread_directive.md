<!-- rye:signed:2026-02-18T05:40:31Z:bcab83ad7609bfd4d4342b74195ee43cdbc27f49a011dc02ea26f78f0a63df8b:jy2mLC3JApzj9vHQzlE9R3pJnQr2PnBQxLrVWdbhb5x4lmiU8FzSQnXMYOoZypxf0gHBDAQgI1CqOlq3v7n_Dw==:440443d0858f0199 -->
# Thread Directive

Execute a directive in a managed thread with an LLM loop.

```xml
<directive name="thread_directive" version="1.0.0">
  <metadata>
    <description>Execute a directive in a managed thread with an LLM loop. Main user-facing directive for thread execution.</description>
    <category>rye/agent</category>
    <author>rye-os</author>
    <model tier="general" />
    <limits max_turns="4" max_tokens="4096" />
    <permissions>
      <execute>
        <tool>rye.agent.threads.thread_directive</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="directive_name" type="string" required="true">
      Fully qualified directive name to execute (e.g., "rye/core/create_directive")
    </input>
    <input name="async_exec" type="boolean" required="false">
      Run asynchronously (default: false). When true, returns immediately with thread_id.
    </input>
    <input name="inputs" type="object" required="false">
      Input parameters to pass to the directive
    </input>
    <input name="model" type="string" required="false">
      Override the model used for thread execution
    </input>
    <input name="limit_overrides" type="object" required="false">
      Override default limits (max_turns, max_tokens, max_spend)
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
    Validate that {input:directive_name} is non-empty and well-formed.
    If empty, halt with an error.
  </step>

  <step name="execute_thread">
    Execute the directive in a managed thread:
    `rye_execute(item_type="tool", item_id="rye/agent/threads/thread_directive", parameters={"directive_name": "{input:directive_name}", "async_exec": {input:async_exec}, "inputs": {input:inputs}, "model": "{input:model}", "limit_overrides": {input:limit_overrides}})`
  </step>

  <step name="return_result">
    Return the thread result containing thread_id, status, cost, and result text.
    If async_exec was true, return immediately with the thread_id and status "running".
  </step>
</process>

<success_criteria>
  <criterion>Directive name is validated as non-empty</criterion>
  <criterion>Thread execution tool called with correct parameters</criterion>
  <criterion>Result includes thread_id, status, cost, and result text</criterion>
</success_criteria>
