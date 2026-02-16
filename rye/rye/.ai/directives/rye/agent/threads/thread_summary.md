<!-- rye:signed:2026-02-16T09:12:32Z:a7f998174bc3cc428b6ece6341c6653b3b22be9158b2f907655cfd93c863226b:O1KUiF53rCbtsiNFY-EWHq13xLuQnKIzE1VxpX8Yv8dzeQDR5H3pPtVzqHZyYFAC3LUW1JNtJm8V9kgskFktCQ==:440443d0858f0199 -->

# Thread Summary

Summarize a thread's conversation for context carryover during thread resumption. Produces a structured summary that fits within a token budget.

```xml
<directive name="thread_summary" version="1.0.0">
  <metadata>
    <description>Summarize a thread conversation for resume context. Returns a structured summary within a token budget.</description>
    <category>rye/agent/threads</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits max_turns="3" max_tokens="8192" max_spend="0.02" />
    <permissions>
      <execute>
        <tool>rye/agent/threads/internal/*</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="transcript_content" type="string" required="true">
      The full or partial transcript content to summarize
    </input>
    <input name="directive_name" type="string" required="true">
      Name of the directive this thread was executing
    </input>
    <input name="max_summary_tokens" type="integer" required="false">
      Target maximum tokens for the summary output (default: 4000)
    </input>
  </inputs>

  <outputs>
    <success>Structured summary of thread conversation</success>
    <failure>Failed to generate thread summary</failure>
  </outputs>
</directive>
```

Summarize the provided thread transcript for context carryover. Your summary will be injected into a resumed thread so the LLM can continue work with full awareness of prior progress.

## Instructions

1. Read the transcript content provided in the input
2. Produce a structured summary with these sections:

### Summary Format

```
## Thread Summary

**Directive:** {directive_name}
**Status:** What state the thread was in when it stopped

### Completed Work
- Bullet list of what was accomplished, including key results and data

### Pending Work  
- What remained to be done when the thread stopped

### Key Decisions & Context
- Important decisions made during execution
- Relevant data/state that the resumed thread needs

### Tool Results (Key Data)
- Important tool outputs that should be preserved verbatim (IDs, scores, structured data)
```

3. Keep the summary concise but preserve:
   - All actionable data (IDs, scores, names, structured results)
   - Decision points and reasoning
   - Error context if the thread errored
4. Stay within the token budget specified by max_summary_tokens
5. Return the summary as your final response text
