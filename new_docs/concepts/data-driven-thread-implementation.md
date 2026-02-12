# Data-Driven Thread Orchestration — Updated Implementation Plan

> All thread behavior driven from YAML configuration. No hardcoded constants.
>
> Consolidated location: `rye/rye/.ai/tools/rye/agent/threads/`

## Design Philosophy

**Everything is data.** Event types, error classification, retry policies, and hook conditions are all defined in YAML configuration files, not Python code. The thread directive loads these configurations at runtime and uses them to drive all behavior.

This mirrors how the core extractors work - they have declarative `EXTRACTION_RULES` and `VALIDATION_SCHEMA` instead of hardcoded logic.

## Configuration File Structure

```
rye/rye/.ai/tools/rye/agent/threads/
├── config/
│   ├── events.yaml              # Event type definitions
│   ├── error_classification.yaml # Error patterns & retry policies
│   ├── hook_conditions.yaml     # Built-in hook templates
│   └── thread_modes.yaml        # Thread mode definitions
├── thread_directive.py          # Loads configs, drives behavior
├── error_classifier.py          # Uses error_classification.yaml
├── hook_evaluator.py            # Uses hook_conditions.yaml
├── event_emitter.py             # Uses events.yaml
└── ...
```

## 1. Event Configuration (data-driven-thread-events.md)

**Replaces:** Python constants like `CRITICAL_TRANSCRIPT_EVENTS = {...}`

**Config:** `rye/rye/.ai/tools/rye/agent/threads/config/events.yaml`

```yaml
schema_version: "1.0.0"

event_types:
  cognition_out:
    category: cognition
    criticality: critical
    payload_schema: {type: object, required: [text], properties: {text: {type: string}}}
    
  cognition_out_delta:
    category: cognition
    criticality: droppable
    emit_config:
      async: true
      throttle: 1s
      condition: provider_config.stream.enabled
```

**Usage:**
```python
# thread_directive.py - no hardcoded event types
emitter = EventEmitter.from_config(project_path)
emitter.emit(thread_id, "cognition_out", {"text": response_text})
# Config determines: is it critical? async or sync? what schema?
```

## 2. Error Classification (data-driven-error-classification.md)

**Replaces:** Procedural `classify_error()` function with hardcoded if/elif chains

**Config:** `rye/rye/.ai/tools/rye/agent/threads/config/error_classification.yaml`

```yaml
schema_version: "1.0.0"

patterns:
  - id: "http_429"
    name: "rate_limited"
    category: "rate_limited"
    retryable: true
    match:
      any:
        - path: "status_code"
          op: "eq"
          value: 429
    retry_policy:
      type: "use_header"
      header: "retry-after"
```

**Usage:**
```python
# error_classifier.py - uses data-driven patterns
classifier = ErrorClassifier.from_config(project_path)
classification = classifier.classify(error, context)
# Returns category, retryable flag, retry policy from config
```

## 3. Hook Evaluation (data-driven-hooks.md)

**Replaces:** Custom expression parser for `when="..."` strings

**Config:** `rye/rye/.ai/tools/rye/agent/threads/config/hook_conditions.yaml`

```yaml
schema_version: "1.0.0"

operators:
  eq: {description: "Equal", types: [string, number, boolean]}
  gt: {description: "Greater than", types: [number]}
  exists: {description: "Path exists", types: [any]}

builtin_hooks:
  - id: "default_limit_escalation"
    event: "limit"
    condition:
      path: "event.limit_code"
      op: "exists"
    action:
      type: "escalate"
      target: "user_approval"
```

**Usage:**
```python
# hook_evaluator.py - uses JSON Path conditions
evaluator = HookEvaluator.from_config(project_path)
action = evaluator.evaluate(event_type, context, directive_hooks)
# Conditions evaluated from config, not hardcoded parser
```

## 4. Thread Modes

**Replaces:** Hardcoded mode logic in thread_directive.py

**Config:** `rye/rye/.ai/tools/rye/agent/threads/config/thread_modes.yaml`

```yaml
schema_version: "1.0.0"

modes:
  single:
    description: "Execute once, complete"
    lifecycle: [running, completed]
    state_persistence: per_turn
    
  conversation:
    description: "Multi-turn with resume capability"
    lifecycle: [running, suspended, completed]
    state_persistence: per_turn
    resume_capability: true
    
  channel:
    description: "Multi-party coordination channel"
    lifecycle: [running, suspended, completed]
    turn_protocols: [round_robin, on_demand]
```

## Implementation Changes

### Phase 1: Configuration Loaders

Create config loader utilities that follow the extractor pattern:

```python
# rye/rye/.ai/tools/rye/agent/threads/config_loader.py

class ThreadConfigLoader:
    """Load thread configuration from YAML, following precedence:
    1. Project-level: .ai/config/thread_*.yaml
    2. System defaults: rye/agent/threads/config/*.yaml
    """
    
    @classmethod
    def load_events(cls, project_path: Path) -> EventConfig:
        return cls._load_config("events.yaml", project_path, EventConfig)
    
    @classmethod
    def load_error_classification(cls, project_path: Path) -> ErrorClassificationConfig:
        return cls._load_config("error_classification.yaml", project_path, ErrorClassificationConfig)
    
    @classmethod
    def load_hooks(cls, project_path: Path) -> HookConfig:
        return cls._load_config("hook_conditions.yaml", project_path, HookConfig)
```

### Phase 2: Refactor thread_directive.py

Remove all hardcoded constants, use config-driven components:

```python
# BEFORE (hardcoded)
CRITICAL_TRANSCRIPT_EVENTS = {..., "cognition_out", ...}
DROPPABLE_TRANSCRIPT_EVENTS = {..., "cognition_out_delta", ...}

async def emit_event(thread_id, event_type, data):
    if event_type in CRITICAL_TRANSCRIPT_EVENTS:
        await sync_write(thread_id, event_type, data)
    elif event_type in DROPPABLE_TRANSCRIPT_EVENTS:
        await async_write(thread_id, event_type, data)

# AFTER (config-driven)
class EventEmitter:
    def __init__(self, config: EventConfig):
        self.config = config
    
    async def emit(self, thread_id, event_type, data):
        event_def = self.config.get_event(event_type)
        if event_def.criticality == "critical":
            await sync_write(thread_id, event_type, data)
        elif event_def.criticality == "droppable":
            await async_write(thread_id, event_type, data)
```

### Phase 3: Refactor error handling

```python
# BEFORE (hardcoded)
def classify_error(error):
    if isinstance(error, RateLimitError):
        return ErrorCategory.RATE_LIMITED
    elif status_code == 429:
        return ErrorCategory.RATE_LIMITED
    # ... more hardcoded patterns

# AFTER (config-driven)
class ErrorClassifier:
    def __init__(self, config: ErrorClassificationConfig):
        self.patterns = config.patterns
    
    def classify(self, error, context) -> Classification:
        match_doc = self._build_match_doc(error, context)
        for pattern in self.patterns:
            if self._matches(match_doc, pattern.match):
                return Classification(
                    category=pattern.category,
                    retry_policy=pattern.retry_policy
                )
        return Classification(**config.default)
```

### Phase 4: Refactor hook evaluation

```python
# BEFORE (custom parser)
def evaluate_hooks(event, context, hooks):
    for hook in hooks:
        if eval(hook.when_expression):  # Custom parser
            return hook.action

# AFTER (JSON Path from config)
class HookEvaluator:
    def __init__(self, config: HookConfig):
        self.operators = config.operators
    
    def evaluate(self, event_type, context, hooks):
        for hook in hooks:
            if hook.event != event_type:
                continue
            if self._matches_condition(context, hook.condition):
                return hook.action
```

## File Organization

### New Files to Create

```
rye/rye/.ai/tools/rye/agent/threads/
├── config/
│   ├── __init__.py
│   ├── events.yaml              # Event type definitions
│   ├── error_classification.yaml # Error patterns
│   ├── hook_conditions.yaml     # Built-in hooks
│   └── thread_modes.yaml        # Thread modes
├── config_loader.py             # Load configs with precedence
├── event_emitter.py             # Config-driven event emission
├── error_classifier.py          # Config-driven error classification
└── hook_evaluator.py            # Config-driven hook evaluation
```

### Files to Modify

```
rye/rye/.ai/tools/rye/agent/threads/
├── thread_directive.py          # Use config-driven components
├── safety_harness.py            # Load limits/hooks from config
└── conversation_mode.py         # Use config-driven state management
```

## Benefits

1. **No hardcoded constants** - All behavior driven from YAML
2. **Project customization** - Projects can extend/override via `.ai/config/`
3. **Type safety** - JSON Schema validation of configs
4. **Testing** - Easy to test with different config files
5. **Documentation** - Config files are self-documenting
6. **Consistency** - Same pattern as core extractors

## Migration Strategy

1. **Phase 1:** Create config files with current hardcoded values
2. **Phase 2:** Create config loaders and validation
3. **Phase 3:** Refactor components to use config (parallel to old code)
4. **Phase 4:** Switch over, remove hardcoded constants
5. **Phase 5:** Remove old code paths

## Configuration Precedence

Configs are loaded in order (later overrides earlier):

1. System defaults: `rye/rye/.ai/tools/rye/agent/threads/config/*.yaml`
2. User space: `~/.ai/config/thread_*.yaml` (optional)
3. Project space: `.ai/config/thread_*.yaml` (optional)

This allows:
- System provides sensible defaults
- Users can set personal preferences
- Projects can customize for their needs
