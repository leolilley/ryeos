<!-- rye:signed:2026-02-25T07:50:41Z:1b9cb53d2ba75c7e4f7dc36e2ae26dc73465b7f0ce53a6e2c351bf0adac2f9e7:0gs9wLzhTBaJ4c6u0IaGS3X9ufgDgBzZtGbrIDnfYyXykLtMBwsvXNp1fG7ShoLICJMITCRPnMmRsvOOTTFcDQ==:9fbfabe975fa5a7f -->
<!-- rye:signed:2026-02-22T02:31:19Z:ccc704fc9d669669ffaab0c32285e141082845c7c34893c92317cb830ec46fd5:yz6NQXL0fa4BSosV6m1sUsNDoqPzJA7IajmFUKe83aimdsOBKbwsM-5N8cwgZeCzTOI0eKYZE0NTuGIbpZByCg==:9fbfabe975fa5a7f -->
# Threading

Guide 7: The threading system — spawning LLM threads, limits, permissions, and orchestration.

```xml
<directive name="threading" version="1.0.0">
  <metadata>
    <description>Guide 7: The threading system — spawning LLM threads, limits, permissions, and orchestration.</description>
    <category>rye/guides</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="20" tokens="10240" />
    <permissions>
      <execute>
        <tool>rye.agent.*</tool>
        <tool>rye.core.system.*</tool>
      </execute>
      <search>
        <tool>*</tool>
      </search>
      <load>
        <tool>*</tool>
      </load>
    </permissions>
  </metadata>

  <inputs />

  <outputs>
    <output name="understanding">User understands the threading system: spawning threads, limits, permissions, orchestration patterns</output>
  </outputs>
</directive>
```

<process>
  <step name="intro">
    <render>
This is where the "one agent, many threads" concept becomes real.

A thread is an independent LLM context that runs a directive. It has its own conversation, its own tool access, its own limits and permissions. But it's the same Rye substrate underneath — same signing, same verification, same execution chain.

You write a directive. Rye spawns a thread. The thread runs the directive with a real LLM. The LLM calls tools, makes decisions, returns results. All within the limits you set.

This requires an LLM provider. Let's check if you have one configured.
</render>
    <instruction>
Output ONLY the text inside the render block above. No step labels, no headers, no preamble, no commentary. Then immediately proceed to the next step — do NOT wait for user input.
</instruction>
  </step>

  <step name="provider_check">
    <instruction>
Search for provider configuration knowledge:
<tool_call>rye_search(scope="knowledge", query="provider configuration")</tool_call>
<tool_call>rye_execute(item_type="knowledge", item_id="rye/agent/provider-configuration")</tool_call>
</instruction>
    <render>
Providers are configured as YAML files in `.ai/tools/rye/agent/providers/`. Rye ships with `anthropic.yaml` and `openai.yaml`. Your API key goes in `~/.ai/.env`:

```
ANTHROPIC_API_KEY=sk-ant-...
# or
OPENAI_API_KEY=sk-...
```
</render>
    <instruction>
If the user doesn't have a provider set up, tell them: "Run `rye execute directive rye/agent/setup_provider` to configure one, then come back here." and stop.
If they do have a provider configured, output the render block and continue to the next step.
</instruction>
  </step>

  <step name="threaded_directives">
    <render>
A threaded directive is a regular directive that runs inside an LLM thread instead of being rendered to your current conversation. The key difference is in the metadata:

```xml
<model tier="haiku" />
<limits turns="10" tokens="50000" spend="0.10" />
<permissions>
  <execute>
    <tool>rye.file-system.*</tool>
  </execute>
</permissions>
```

The model tier tells Rye which LLM to use. The limits set hard ceilings. The permissions control what tools the thread can access.

To create one from scratch:
</render>
    <instruction>
Output the render block above, then show the user how to create a threaded directive:
<tool_call>rye execute directive rye/agent/threads/create_threaded_directive</tool_call>
Continue to the next step without waiting for user input.
</instruction>
  </step>

  <step name="spawning">
    <render>
Spawn a thread with `thread_directive`:
</render>
    <instruction>
Output the render block, then show the tool call example:
<tool_call>rye_execute(item_type="tool", item_id="rye/agent/threads/thread_directive", parameters={"directive_id": "my-directive", "parameters": {"key": "value"}})</tool_call>
Then output the following render block and continue.
</instruction>
    <render>
This returns a `thread_id` immediately — the thread runs asynchronously. The thread gets its own conversation with the LLM, its own tool access, its own safety harness.

Thread IDs follow the pattern `{directive_name}-{epoch}`, e.g., `my-directive-1739820456`. Human-readable and unique.
</render>
  </step>

  <step name="streaming">
    <render>
Watch a thread in real-time:

```bash
tail -f .ai/threads/<thread_id>/transcript.jsonl
```

Each thread writes two outputs:
- `transcript.jsonl` — token-by-token JSONL events (for programmatic consumption)
- Knowledge markdown — the full conversation in readable form

The `TranscriptSink` handles both simultaneously. Every token the LLM produces, every tool call, every result — streamed and persisted.
</render>
    <instruction>
Output the render block above exactly. Continue to the next step.
</instruction>
  </step>

  <step name="limits">
    <render>
Every thread runs inside a `SafetyHarness` that enforces limits:

| Limit | Controls |
|-------|----------|
| `turns` | Max LLM conversation turns |
| `tokens` | Max total tokens (input + output) |
| `spend` | Max USD spend for this thread |
| `duration_seconds` | Max wall-clock execution time |
| `spawns` | Max child threads this thread can spawn |
| `depth` | Max remaining nesting depth |

Limits resolve in four layers:
```
resilience.yaml defaults → directive metadata → limit_overrides → parent upper bounds
```

The parent upper bound is the hard ceiling. A child thread can never exceed its parent's remaining budget. Spend $0.30 of a $0.50 limit? Your child gets at most $0.20.
</render>
    <instruction>
Output the render block above exactly, including the table and the code block. Continue to the next step.
</instruction>
  </step>

  <step name="permissions">
    <render>
Capability tokens control what each thread can access:

```
rye.<primary>.<item_type>.<item_id_dotted>
```

Examples:
- `rye.execute.tool.rye.file-system.*` — execute any file-system tool
- `rye.search.directive.*` — search all directives
- `rye.load.knowledge.my-project.*` — load knowledge under my-project/

Declared in directive XML, enforced by the safety harness. The critical rule: **capabilities only attenuate down the hierarchy**. A child thread can never have MORE permissions than its parent. Parent has `rye.execute.tool.rye.file-system.*`? Child can have `rye.execute.tool.rye.file-system.write` but NOT `rye.execute.tool.rye.bash.*`.
</render>
    <instruction>
Output the render block above exactly. Continue to the next step.
</instruction>
  </step>

  <step name="orchestration">
    <render>
The real power: spawning threads from threads.

An orchestrator directive spawns multiple child threads, waits for them, collects results:

```
rye_execute(item_type="tool", item_id="rye/agent/threads/orchestrator",
  parameters={
    "threads": [
      {"directive_id": "task-a", "parameters": {"input": "..."}},
      {"directive_id": "task-b", "parameters": {"input": "..."}}
    ],
    "wait": true
  })
```

Three roles in a hierarchy:

| Role | Model | Purpose | Spawns? |
|------|-------|---------|---------|
| Orchestrator | sonnet | Coordinate workflow, manage state | Yes |
| Sub-orchestrator | sonnet | Coordinate a phase | Yes |
| Execution leaf | haiku | Call one tool, return result | No |

Rule of thumb: if it spawns children → orchestrator. If it calls a tool and returns → leaf.
</render>
    <instruction>
Output the render block above exactly, including both the code block and the table. Continue to the next step.
</instruction>
  </step>

  <step name="next">
    <render>
One agent. Many threads. Each with its own limits, permissions, and LLM context. Orchestrators that spawn sub-orchestrators that spawn execution leaves. All signed, all verified, all within budget.

Last stop — declarative state graphs:

```
rye execute directive graphs
```
</render>
    <instruction>
Output the render block above exactly. This is the final step — stop here.
</instruction>
  </step>
</process>

<success_criteria>
<criterion>User understands threads as independent LLM contexts running directives</criterion>
<criterion>Provider configuration explained and checked</criterion>
<criterion>Threaded directive metadata (model, limits, permissions) explained</criterion>
<criterion>Thread spawning and thread IDs explained</criterion>
<criterion>Streaming and transcript output explained</criterion>
<criterion>SafetyHarness limits and resolution layers explained</criterion>
<criterion>Capability token format and attenuation rule explained</criterion>
<criterion>Orchestration patterns (orchestrator, sub-orchestrator, leaf) explained</criterion>
<criterion>User directed to the graphs guide as next step</criterion>
</success_criteria>
