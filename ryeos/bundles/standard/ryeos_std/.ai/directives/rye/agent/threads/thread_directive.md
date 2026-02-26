<!-- rye:signed:2026-02-25T07:50:41Z:5690976a2346861f377798c2e4429d0df3c92037b20d5ebaab2fd9b8db0da1e8:U7UrUbcp0qSIh2RMDfJkZFAqLVKGnFTqZYZB6qsM01GbYmIRi_hKUD16KSToakjD1QkpcjB-JLXjmUGH7LtXDA==:9fbfabe975fa5a7f -->
<!-- rye:signed:2026-02-22T02:31:19Z:b596181f60885e1e7e9e00a4d0fe1f753343215c47ac3fb7f4935c0ef98fc450:yVQHXx-9JNgHonX1Lrxj6eIWmWF_-fEBt_MAMtlCczT1k-bSTwPxwNyIwHsgzd4F2XyOxSSZB1Hpxayy6h8wAg==:9fbfabe975fa5a7f -->
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
    `rye_execute(item_type="tool", item_id="rye/agent/threads/thread_directive", parameters={"directive_id": "{input:directive_id}", "async": {input:async}, "inputs": {input:inputs}, "model": "{input:model}", "limit_overrides": {input:limit_overrides}})`
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
