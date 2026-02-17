**Source:** Original implementation: `.ai/tools/rye/threads/` in kiwi-mcp

# Threads Category

## Purpose

Thread tools provide **async execution and thread management** for asynchronous operations.

**Location:** `.ai/tools/rye/threads/`  
**Count:** 12 tools + YAML configs  
**Executor:** All use `python_runtime`

## Core Thread Operations

### Thread Management (4 tools)

#### 1. Create Thread (`thread_create.py`)
Create new execution thread

#### 2. Read Thread (`thread_read.py`)
Retrieve thread information

#### 3. Update Thread (`thread_update.py`)
Update thread metadata

#### 4. Delete Thread (`thread_delete.py`)
Delete thread and associated data

### Message Operations (4 tools)

#### 5. Add Message (`message_add.py`)
Add message to thread

#### 6. Read Messages (`message_read.py`)
Retrieve thread messages

#### 7. Update Message (`message_update.py`)
Update message content

#### 8. Delete Message (`message_delete.py`)
Delete message from thread

### Run Operations (4 tools)

#### 9. Create Run (`run_create.py`)
Create execution run

#### 10. Read Run (`run_read.py`)
Get run status and results

#### 11. Update Run (`run_update.py`)
Update run configuration

#### 12. Read Run Steps (`run_step_read.py`)
Retrieve run step details

## Thread Architecture

```
User/Tool
    │
    └─→ Create Thread
        │
        ├─→ Add Messages
        ├─→ Create Run
        ├─→ Read Run Steps (execution)
        │
        └─→ Read Thread
            ├─ See all messages
            ├─ See all runs
            └─ See complete history
```

## YAML Configurations

Thread tools support different provider backends via YAML:

### Anthropic Thread Config (`anthropic_thread.yaml`)

```yaml
name: anthropic_thread
version: "1.0.0"
provider: anthropic
config:
  model: claude-3-opus-20240229
  max_tokens: 4096
  tools: []
  system_prompt: ""
```

### OpenAI Thread Config (`openai_thread.yaml`)

```yaml
name: openai_thread
version: "1.0.0"
provider: openai
config:
  model: gpt-4-turbo
  temperature: 0.7
  max_tokens: 4096
  tools: []
```

## Metadata Pattern

All thread tools follow this pattern:

```python
# .ai/tools/rye/threads/{name}.py

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "python_runtime"
__category__ = "threads"

CONFIG_SCHEMA = { ... }

def main(**kwargs) -> dict:
    """Thread operation."""
    pass
```

## Usage Examples

### Create Thread

```bash
Call thread_create with:
  name: "my_conversation"
  metadata:
    user: "user@example.com"
    project: "my_project"
```

### Add Message

```bash
Call message_add with:
  thread_id: "thread-123"
  content: "Hello, how can I help?"
  role: "user"
```

### Create Run

```bash
Call run_create with:
  thread_id: "thread-123"
  model: "claude-3-opus-20240229"
  instructions: "You are a helpful assistant"
```

### Read Thread

```bash
Call thread_read with:
  thread_id: "thread-123"
  include_messages: true
  include_runs: true
```

## Thread Workflow

```
1. Create Thread
   thread_create() → thread-123

2. Add Messages
   message_add(thread-123, "Hello")
   message_add(thread-123, "How are you?")

3. Create Run
   run_create(thread-123, model="claude-3-opus") → run-456

4. Monitor Run
   run_read(thread-123, run-456) → status "in_progress"
   run_step_read(thread-123, run-456) → step details

5. Read Completed Thread
   thread_read(thread-123) → all messages + all runs
   message_read(thread-123) → all messages
```

## Key Characteristics

| Aspect | Detail |
|--------|--------|
| **Count** | 12 tools |
| **Location** | `.ai/tools/rye/threads/` |
| **Executor** | All use `python_runtime` |
| **Purpose** | Async execution & thread management |
| **Providers** | Anthropic, OpenAI |
| **Operations** | CRUD for threads, messages, runs |

## Related Documentation

- [overview](../overview.md) - All categories
- [agent/llm](llm.md) - LLM provider configs
- [../bundle/structure](../bundle/structure.md) - Bundle organization
