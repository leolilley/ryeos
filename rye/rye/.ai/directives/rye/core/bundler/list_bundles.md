<!-- rye:signed:2026-02-18T05:40:31Z:6c67607a78e1c4b2bc6c054a2455968247fae76beddc95c2bcd23c87496c38cf:jMBFyHXOLVwKdU-ixK9tDhgsbLe0i5Bhbx3DSCmeMlyJ49tS8u1Nog1K7Y3ytGvpgMW7WuFbqgFNWCzziYYtDQ==:440443d0858f0199 -->
# List Bundles

List all available bundles.

```xml
<directive name="list_bundles" version="1.0.0">
  <metadata>
    <description>Wraps bundler.py action=list to list all available bundles.</description>
    <category>rye/core/bundler</category>
    <author>rye-os</author>
    <model tier="haiku" />
    <limits max_turns="3" max_tokens="2048" />
    <permissions>
      <execute>
        <tool>rye.core.bundler.*</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs />

  <outputs>
    <output name="bundles">List of available bundles</output>
  </outputs>
</directive>
```

<process>
  <step name="call_bundler_list">
    Call the bundler tool with action=list.
    `rye_execute(item_type="tool", item_id="rye/core/bundler/bundler", parameters={"action": "list"})`
  </step>

  <step name="return_result">
    Return the list of bundles to the user.
  </step>
</process>
