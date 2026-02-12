# Data-Driven State Persistence

> Schema and configuration for thread state checkpoints (state.json)
>
> **Location:** `.ai/threads/{thread_id}/state.json`

## Overview

Thread state is persisted to `state.json` at configurable checkpoints for crash recovery, suspension/resume, and conversation continuation. The schema and checkpoint triggers are data-driven from YAML configuration.

## State Schema

```yaml
# state_schema.yaml
schema_version: "1.0.0"

state:
  # Required fields (must be present)
  required:
    - thread_id
    - directive
    - version
    - saved_at

  # Full schema definition
  schema:
    type: object
    required: [thread_id, directive, version, saved_at]
    properties:
      # Identity
      thread_id:
        type: string
        description: "Unique thread identifier"

      directive:
        type: string
        description: "Directive name being executed"

      parent_thread_id:
        type: [string, "null"]
        description: "Parent thread ID for child threads"
        default: null

      # Versioning
      version:
        type: string
        description: "State format version"
        default: "1.0.0"

      saved_at:
        type: string
        format: date-time
        description: "ISO 8601 timestamp of save"

      # Execution State
      inputs:
        type: object
        description: "Original directive inputs"

      turn_number:
        type: integer
        description: "Current turn number (0-indexed)"
        default: 0

      # Cost Tracking
      cost:
        type: object
        properties:
          turns:
            type: integer
            default: 0
          tokens:
            type: object
            properties:
              input_tokens:
                type: integer
                default: 0
              output_tokens:
                type: integer
                default: 0
          spend:
            type: number
            default: 0.0
          duration_seconds:
            type: number
            default: 0.0

      # Limits
      limits:
        type: object
        properties:
          spend:
            type: [number, "null"]
            description: "Maximum spend in dollars"
          turns:
            type: [integer, "null"]
            description: "Maximum turns"
          tokens:
            type: [integer, "null"]
            description: "Maximum tokens"
          spawns:
            type: [integer, "null"]
            description: "Maximum child threads"

      # Suspension State
      status:
        type: string
        enum: [running, suspended, completed, error, cancelled]
        description: "Thread status at save time"

      suspend_reason:
        type: [string, "null"]
        enum: [limit, error, budget, approval, null]
        description: "Why thread was suspended (null if not suspended)"

      suspend_metadata:
        type: [object, "null"]
        description: "Additional context for suspension"
        properties:
          limit_code:
            type: string
            enum:
              [
                turns_exceeded,
                tokens_exceeded,
                spend_exceeded,
                spawns_exceeded,
                duration_exceeded,
              ]
          current_value:
            type: number
          current_max:
            type: number

      # Hooks & Permissions
      hooks:
        type: array
        items:
          type: object
          properties:
            event:
              type: string
            directive:
              type: string
            inputs:
              type: object

      required_caps:
        type: array
        items:
          type: string
        description: "Required capability tokens"

      # Context
      messages:
        type: array
        description: "Conversation messages for reconstruction"
        items:
          type: object
          properties:
            role:
              type: string
              enum: [system, user, assistant]
            content:
              type: string

      partial_cognition:
        type: [object, "null"]
        description: "Partial LLM output if interrupted"
        properties:
          text:
            type: string
          is_partial:
            type: boolean
          completion_percentage:
            type: number

# Checkpoint Configuration
checkpoint:
  # When to save state
  triggers:
    pre_turn:
      enabled: true
      description: "Before LLM call"
      priority: 1

    post_llm:
      enabled: true
      description: "After LLM response (cost updated)"
      priority: 2

    post_tools:
      enabled: true
      description: "After tool execution"
      priority: 3

    on_suspend:
      enabled: true
      description: "When thread suspends"
      priority: 10 # Highest - must save

    on_error:
      enabled: true
      description: "When error occurs"
      priority: 10

    on_cancel:
      enabled: true
      description: "When cancellation requested"
      priority: 10

  # Performance tuning
  performance:
    # Batching: only save changed fields?
    incremental: false # For now, full saves only

    # Compression
    compress: false

    # Atomic writes
    atomic: true # Write to .tmp then rename

    # Retry on write failure
    retry:
      enabled: true
      max_attempts: 3
      backoff: "exponential"

# Retention Policy
retention:
  # How many checkpoints to keep
  max_checkpoints: 10

  # Cleanup old checkpoints
  cleanup:
    enabled: true
    keep_last_n: 3 # Always keep last 3
    delete_older_than_days: 7

  # On thread completion
  on_completion:
    action: "archive" # archive | delete | keep
    archive_path: ".ai/threads/{thread_id}/archive/"
```

## Example State.json

```json
{
  "thread_id": "planner-1739012630",
  "directive": "apps/task-manager/build_crud_app",
  "version": "1.0.0",
  "saved_at": "2026-02-12T15:30:00Z",

  "parent_thread_id": null,

  "inputs": {
    "app_name": "Task Manager",
    "features": ["crud", "auth"]
  },

  "turn_number": 3,

  "cost": {
    "turns": 3,
    "tokens": {
      "input_tokens": 4500,
      "output_tokens": 3200
    },
    "spend": 0.045,
    "duration_seconds": 45.5
  },

  "limits": {
    "spend": 1.0,
    "turns": 10,
    "tokens": 100000,
    "spawns": 5
  },

  "status": "suspended",
  "suspend_reason": "limit",
  "suspend_metadata": {
    "limit_code": "spend_exceeded",
    "current_value": 1.05,
    "current_max": 1.0
  },

  "hooks": [
    {
      "event": "limit",
      "directive": "my_app/custom_limit_handler",
      "inputs": {}
    }
  ],

  "required_caps": ["rye.execute.tool.*"],

  "messages": [
    { "role": "system", "content": "You are building an app..." },
    { "role": "assistant", "content": "I'll help you build..." }
  ],

  "partial_cognition": {
    "text": "Let me analyze the requirements...",
    "is_partial": true,
    "completion_percentage": 35
  }
}
```

## Usage

```python
# Load checkpoint config
config = load_config("state_schema.yaml", project_path)

# Save state at checkpoint
async def save_checkpoint(thread_id, harness, trigger):
    # Check if this trigger is enabled
    trigger_config = config.checkpoint.triggers.get(trigger)
    if not trigger_config or not trigger_config.enabled:
        return

    # Build state from schema
    state = {
        "thread_id": thread_id,
        "directive": harness.directive,
        "version": config.state.schema.version,
        "saved_at": datetime.now(timezone.utc).isoformat(),
        "turn_number": harness.cost.turns,
        "cost": harness.cost.to_dict(),
        "limits": harness.limits,
        "status": harness.status,
        "suspend_reason": harness.suspend_reason,
        "messages": harness.messages,
    }

    # Validate against schema
    validate(state, config.state.schema)

    # Atomic write
    state_path = Path(f".ai/threads/{thread_id}/state.json")
    await atomic_write(state_path, state)

# Resume from state
async def resume_from_state(thread_id):
    state_path = Path(f".ai/threads/{thread_id}/state.json")

    with open(state_path) as f:
        state = json.load(f)

    # Validate
    validate(state, config.state.schema)

    # Rebuild harness
    harness = SafetyHarness.from_dict(state)

    return harness
```

## Project Overrides

Projects can customize checkpoint behavior:

```yaml
# .ai/config/state_persistence.yaml
extends: "rye/agent/threads/config/state_schema.yaml"

checkpoint:
  triggers:
    pre_turn:
      enabled: false # Skip pre-turn saves for performance

    post_llm:
      enabled: true

  performance:
    incremental: true # Only save changed fields
    compress: true # Compress state.json

retention:
  max_checkpoints: 20 # Keep more history
  cleanup:
    keep_last_n: 5
```
