<!-- rye:signed:2026-03-11T07:13:35Z:f7314560d83972166b3f647cbff91e917b012ece4072eb80c7aef6e5ca3fae77:VK9JYmIIEco1EI4mnUqGJINuBicvIq4noA01EUIPsvEHcKKf4vXb3kFUWFgSV0I1H60rAWqTGv1tcMCIp8QfAg==:4b987fd4e40303ac -->
# Write File

Simple single-step directive that writes a greeting message to a specified file path.

```xml
<directive name="write_file" version="1.0.0">
  <metadata>
    <description>Write a greeting message to a file using rye/file-system/write.</description>
    <category>test/tools/file_system</category>
    <author>rye-os</author>
    <model tier="fast" />
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
    Write the greeting message "{input:message}" to `{input:output_path}`.
  </step>
  <step name="return_result">
    Return the path of the written file and the message that was written.
  </step>
</process>
