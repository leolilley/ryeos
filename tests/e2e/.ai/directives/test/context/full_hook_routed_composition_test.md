<!-- ryeos:signed:2026-03-29T06:13:34Z:a3367508d7eef643ec64708266e3849f680e2757b20aba8ca4be47385f956dc7:468IRPPXfxwVpfw0fmBmZyVPLgb-S79AOs1DdV8VTF3X-br8ve2Rg5xa7ITnE6tVF3X9T3zbMzdwE_GiypZUBA==:4b987fd4e40303ac -->
# Full 3-Layer Composition Test

Tests all 3 layers working together:
- Layer 1 (Tool Schema Preload): Permissions grant file-system read/write → schemas preloaded
- Layer 2 (Hook Routing): No explicit extends, but category "test/hookroute" triggers
  the resolve_extends hook to route into hook_routed_base
- Layer 3 (Extends Chain): hook_routed_base provides system context (base-identity)
  and before context (hook-routed-rules)

The thread should see:
- BASE_IDENTITY_PRESENT in system prompt (from extends chain)
- HOOK_ROUTED_RULES_PRESENT in before context (from extends chain)
- Tool schemas for rye/file-system/read and rye/file-system/write (from permissions)
- PROJECT_HOOK_TEST_FINDINGS in after context (from project hook)
- No rye/bash/bash schema (not permitted)

```xml
<directive name="full_hook_routed_composition_test" version="1.0.0">
  <metadata>
    <description>Tests full 3-layer composition: tool preload + hook routing + extends chain.</description>
    <category>test/context</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="4" tokens="4096" spend="0.10" />
    <permissions>
      <execute>
        <tool>rye.file-system.read</tool>
        <tool>rye.file-system.write</tool>
      </execute>
    </permissions>
  </metadata>

  <outputs>
    <result>Report confirming all 3 layers are active with expected markers and schemas</result>
  </outputs>
</directive>
```

<process>
  <step name="report_all_layers">
    <description>Report everything you can see in your context:
1. System context markers (check for BASE_IDENTITY_PRESENT)
2. Before context markers (check for HOOK_ROUTED_RULES_PRESENT)
3. Tool schemas preloaded (list which tool item_ids have schemas visible)
4. After context markers (check for PROJECT_HOOK_TEST_FINDINGS)
5. Confirm rye/bash/bash schema is NOT present
Write a complete report to a file.</description>
    <execute item_type="tool" item_id="rye/file-system/write">
      <param name="path" value="outputs/context_full_composition_result.txt" />
      <param name="content" value="Full 3-layer composition report" />
      <param name="create_dirs" value="true" />
    </execute>
  </step>
</process>
