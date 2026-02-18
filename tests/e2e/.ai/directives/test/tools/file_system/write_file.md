<!-- rye:signed:2026-02-18T06:35:44Z:8a02f494e9af52a543c8e9db455d6f659926322b982c681be1ef290a93e30fd0:F2TAoXXMTZ1_H7JARyGp1vZZN5GgPjXaPxv6Dme50CEhFNQ7nFEv-jLwbQ-3Yb0LP2BbcRsOoMJjlVfgPCMHCw==:440443d0858f0199 -->
# Write File

Simple single-step directive that writes a greeting message to a specified file path.

```xml
<directive name="write_file" version="1.0.0">
  <metadata>
    <description>Write a greeting message to a file using rye/file-system/write.</description>
    <category>test/tools/file_system</category>
    <author>rye-os</author>
    <model tier="haiku" />
    <limits turns="3" tokens="4096" />
    <permissions>
      <execute><tool>rye.file-system.*</tool></execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="message" type="string" required="true">
      The greeting message to write.
    </input>
    <input name="output_path" type="string" required="true">
      The file path to write the message to.
    </input>
  </inputs>

  <outputs>
    <output name="path">Path to the written file</output>
    <output name="message">The message that was written</output>
  </outputs>
</directive>
```

<process>
  <step name="write_message">
    Write the greeting message to the output file:
    `rye_execute(item_type="tool", item_id="rye/file-system/write", parameters={"file_path": "{input:output_path}", "content": "{input:message}"})`
  </step>

  <step name="return_result">
    Return the path of the written file and the message that was written.
  </step>
</process>
