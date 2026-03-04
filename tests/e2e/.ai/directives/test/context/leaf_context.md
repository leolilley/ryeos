<!-- rye:signed:2026-03-04T00:05:20Z:2c4c8a892963d721c69bc386400286d947b3b83406c3bb3d79dc274754e995b6:IkfdfWxfT1VFtf-PfOooN1VZI1Bl7WoAgvzH4HCvcGg6HYR1s4ODwzZPmyBbqqyXdi_wFW_zBhB1-1PtDf5IDA==:4b987fd4e40303ac -->
# Leaf Context Directive

Leaf of extends chain. Adds after-context. Should receive base system + mid before + own after.

```xml
<directive name="leaf_context" version="1.0.0" extends="test/context/mid_context">
  <metadata>
    <description>Leaf directive in 3-level extends chain. Tests full context composition.</description>
    <category>test/context</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="4" tokens="4096" spend="0.10" />
    <context>
      <after>test/context/leaf-checklist</after>
    </context>
  </metadata>

  <outputs>
    <result>Confirmation that leaf executed with full composed context from chain</result>
  </outputs>
</directive>
```

<process>
  <step name="write_marker">
    <description>Write a marker file confirming this directive ran.</description>
    <execute item_type="tool" item_id="rye/file-system/write">
      <param name="path" value="outputs/context_leaf_marker.txt" />
      <param name="content" value="leaf_context executed" />
      <param name="create_dirs" value="true" />
    </execute>
  </step>
</process>
