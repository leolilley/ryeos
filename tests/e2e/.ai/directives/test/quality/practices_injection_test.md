<!-- rye:signed:2026-03-11T07:13:35Z:b0620482668504e0f45009aed754c231520834fccf691215efe3aed350780651:-TSe2rUrXt-Hgip7FtEm-U9M6VT9AA4XI5E1z0MN6uV4MTuc-t_bDtZH75yWc6T-ayhoQ7G5CDZHE9WxTWmpCw==:4b987fd4e40303ac -->
<!-- rye:unsigned -->

# Practices Injection Test

End-to-end test for knowledge context injection — verifies that the practices knowledge is available when referenced in a directive's context block.

```xml
<directive name="practices_injection_test" version="1.0.0">
  <metadata>
    <description>Test that anti-slop practices knowledge is injected into directive context via extends chain.</description>
    <category>test/quality</category>
    <author>rye-os</author>
    <model tier="fast" provider="zen/zen" />
    <limits turns="6" tokens="8192" spend="0.10" />
    <context>
      <before>rye/code/quality/practices</before>
    </context>
    <permissions>
      <execute>
        <tool>rye.file-system.*</tool>
      </execute>
      <fetch>*</fetch>
    </permissions>
  </metadata>

  <inputs>
    <input name="output_dir" type="string" required="false">Directory for test output (default: outputs)</input>
  </inputs>

  <outputs>
    <output name="result">Confirmation that practices knowledge was present in context</output>
  </outputs>
</directive>
```

<process>
  <step name="check_practices_context">
    You should have the anti-slop coding practices in your context (injected via the `before` context reference to `rye/code/quality/practices`).
    List the 8 practice rules you can see:
    1. Follow Existing Patterns
    2. Minimal Diffs
    3. No Over-Engineering
    4. No Unnecessary Abstractions
    5. Test With Real Implementations
    6. All Tests Pass Before Handoff
    7. No Dead Code
    8. Style Consistency
    If you can see all 8, the injection worked.
  </step>

  <step name="write_result">
    Write the confirmation to {input:output_dir|outputs}/practices_injection_test.txt.
    Include: which practices were visible in your context, and whether all 8 were present.
    `rye_execute(item_type="tool", item_id="rye/file-system/write", parameters={"path": "{input:output_dir|outputs}/practices_injection_test.txt", "content": "<injection verification>", "create_dirs": true})`
  </step>
</process>

<success_criteria>
  <criterion>All 8 anti-slop practices are visible in the directive's context</criterion>
  <criterion>Practices content matches the knowledge entry rye/code/quality/practices</criterion>
</success_criteria>
