<!-- ryeos:signed:2026-08-11T02:28:28Z:ff87f12c1a560e35b5ea24c59f6333eda2c8ceb967f37e8a890bf974bb20e072:BiW2S2+URqOc4ajlmaV5DF30x0Ae6xrNzIR46OsjLgo3gACenEpg4dYJdQtei/H1XnRgNA9PrPd+w53DocRFCw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
description: "Summarize a thread conversation for resume context. Returns a structured summary within a token budget."
version: "1.0.0"
model:
  tier: fast
limits:
  turns: 3
  tokens: 8192
  spend_usd: "0.02"
requires:
  capabilities:
    declared:
      - ryeos.execute.tool.ryeos/threads/internal/*
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
