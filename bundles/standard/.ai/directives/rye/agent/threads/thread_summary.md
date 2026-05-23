<!-- ryeos:signed:2026-05-22T19:55:06Z:25963c727541ca40c9e299601a66d291d9dc57d6235d139013bf42f4f720034f:Xcob7drIyee0Gax8w+tLclygFL/9fyUSpp0VXT5GIa/Vs0bxMLx435ETneAlwGokPo933HN+rmd3wZ7a0+BKCQ==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->
---
description: "Summarize a thread conversation for resume context. Returns a structured summary within a token budget."
version: "1.0.0"
model_tier: fast
limits:
  turns: 3
  tokens: 8192
  spend: 0.02
permissions:
  execute:
    - tool:rye/agent/threads/internal/*
---

# Thread Summary

Summarize a thread's conversation for context carryover during thread resumption. Produces a structured summary that fits within a token budget.

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
