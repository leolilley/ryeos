<!-- rye:signed:2026-02-13T08:59:49Z:12581daf13fca480f0c2309aa89ffe24c90ff0b7f4f46d1aea6dd6218d3f486a:mtfpwNox7kg15WbQfM_SVvQ7qXkyM9YCryH1UGmrpF3sndjgfYZyujZr005PXhvMM7nUSYBy4zd6LD1Sbtk6Ag==:440443d0858f0199 -->
# Write File

Simple single-step directive that writes a greeting message to a specified file path.

```xml
<directive name="write_file" version="1.0.0">
  <metadata>
    <description>Write a greeting message to a file using fs_write.</description>
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

  <process>
    <step name="write_message">
      <description>Write the greeting message to the output file.</description>
      <execute item_type="tool" item_id="rye/file-system/fs_write">
        <param name="path" value="{input:output_path}" />
        <param name="content" value="{input:message}" />
        <param name="mode" value="overwrite" />
      </execute>
    </step>
  </process>

  <outputs>
    <success>Message written to {input:output_path}.</success>
  </outputs>
</directive>
```
