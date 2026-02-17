# Permission Format Change Proposal

## Current State

### XML Format (directives)

```xml
<permissions>
  <cap>rye.execute.tool.rye.agent.capabilities.primitives.git</cap>
  <cap>rye.execute.tool.rye.file-system.*</cap>
</permissions>
```

### YAML Format (capability definitions)

```yaml
capabilities:
  - "rye.execute.tool.rye.agent.capabilities.primitives.git"
  - "rye.execute.tool.rye.file-system.*"
```

**Issues with current format:**

1. Verbose - Repeats full hierarchy
2. Flat structure - Doesn't leverage XML child element capabilities
3. No clear grouping of permission types
4. Not semantic - All caps look the same regardless of type

this can also change

/home/leo/projects/rye-os/rye/rye/.ai/tools/rye/agent/capabilities/primitives

folder structure should also better reflect our new format and the structure of tools/directives/knowledge

something like capabilties/tools/ capabilties/directives capabilties/knowledge/

## Proposed Format

### XML Format (directives)

```xml
<permissions>
  <execute>
    <tool>rye.agent.capabilities.primitives.git</tool>
    <tool>rye.file-system.*</tool>
  </execute>
  <search>*</search>
  <load>
    <tool>*</tool>
  </load>
  <sign>*</sign>
</permissions>
```

### YAML Format (capability definitions)

```yaml
capabilities:
  - primary: execute
    item_type: tool
    specifics: rye.agent.capabilities.primitives.git

  - primary: execute
    item_type: tool
    specifics: rye.file-system.*

  - primary: load
    item_type: tool
    specifics: "*"
```

## Benefits

1. **Hierarchical**: Uses XML child elements to represent tree structure naturally
2. **Not verbose**: "rye" prefix appears once at each permission level, not at every capability
3. **Encapsulated**: Each permission type (execute, search, load, sign) is self-contained
4. **Clear semantics**: Root element clearly indicates permission type
5. **Tight wildcards**: Uses `*` suffix for readability without repetition
6. **Type-specific**: Differentiates between tool/directive/knowledge permissions

## Examples

### Single Capability

```xml
<permissions>
  <execute>
    <tool>rye.file-system.fs_write</tool>
  </execute>
</permissions>
```

### Wildcard Permissions

```xml
<permissions>
  <execute>
    <tool>rye.agent.*</tool>
  </execute>
  <search>*</search>
  <load>*</load>
  <sign>*</sign>
</permissions>
```

### Full Access (God Mode)

```xml
<permissions>*</permissions>
```

### Execute Only

```xml
<permissions>
  <execute>*</execute>
</permissions>
```

### Search Only

```xml
<permissions>
  <search>*</search>
</permissions>
```

### Combined Access

```xml
<permissions>
  <execute>
    <tool>rye.file-system.*</tool>
    <tool>rye.agent.threads.spawn_thread</tool>
  </execute>
  <search>
    <directive>analysis/*</directive>
    <tool>rye.registry.*</tool>
  </search>
  <load>
    <tool>rye.shell.*</tool>
  </load>
  <sign>
    <tool>rye.execute.*</tool>
  </sign>
</permissions>
```

### Specific Item Types

```xml
<permissions>
  <execute>
    <tool>rye.file-system.*</tool>
  </execute>
  <search>
    <directive>workflow/*</directive>
  </search>
  <load>
    <tool>rye.shell.*</tool>
  </load>
  <sign>
    <tool>rye.execute.*</tool>
  </sign>
</permissions>
```

Each permission type can independently specify which item types it can access.

## Implementation Requirements

### 1. Directive Extractor Update (`rye/core/extractors/directive/directive_extractor.py`)

- Update `EXTRACTION_RULES` to handle new XML structure
- Support nested XML elements with flattened values

### 2. Thread Directive Update (`rye/agent/threads/thread_directive.py`)

- Update `_extract_caps_from_permissions()` to parse new format
- Extract capabilities from `<execute>`, `<search>`, `<load>`, `<sign>` children

### 3. Validator Update

- Update schema validation to recognize new permission structure
- Ensure backward compatibility during transition

### 4. Migration Strategy

- Support both old and new formats during transition period
- Provide migration tooling for converting existing directives
- Document deprecation timeline

### 5. Examples Update

- Update all directive examples in README and knowledge files
- Update create_directive and create_advanced_directive directives
- Add migration guide

## Backward Compatibility

During transition, the system should:

1. Accept both old (`<cap>rye.execute.tool.*</cap>`) and new (`<execute><tool>*</tool></execute>`) formats
2. Warn about deprecated format usage
3. Auto-migrate to new format when possible
4. Provide clear migration guide

## Files to Update

1. `rye/core/extractors/directive/directive_extractor.py`
2. `rye/agent/threads/thread_directive.py`
3. `rye/rye/.ai/knowledge/rye/core/directive-metadata-reference.md`
4. `README.md`
5. `rye/rye/.ai/directives/rye/core/create_directive.md`
6. `rye/rye/.ai/directives/rye/core/create_advanced_directive.md`

## Timeline

- **Phase 1**: Implement parser for new format, maintain dual support
- **Phase 2**: Add migration tooling and deprecation warnings
- **Phase 3**: Update examples and documentation
- **Phase 4**: Remove old format support
