<!-- rye:signed:2026-02-26T06:42:50Z:0d7794f2d8126a50aecc05bbd0ffbd1c606653bc62d48e38925cf0c0e8ac52d6:6extz_xe6XsdURwd5-AaR_3zkN_uy_B_Lc2t754HvqA5UaW-cr7TWAZCuSjNecYbMhn7wEyQJ2NqeNMj6weMCg==:4b987fd4e40303ac -->

# Thread Summary

Summarize a thread's conversation for context carryover during thread resumption. Produces a structured summary that fits within a token budget.

```xml
<directive name="thread_summary" version="1.0.0">
  <metadata>
    <description>Summarize a thread conversation for resume context. Returns a structured summary within a token budget.</description>
    <category>rye/agent/threads</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="3" tokens="8192" spend="0.02" />
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
