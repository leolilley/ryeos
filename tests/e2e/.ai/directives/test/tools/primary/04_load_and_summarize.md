<!-- rye:validated:2026-02-10T03:00:00Z:placeholder -->

# Load and Summarize Directive

Load a directive to inspect its structure, then write a summary of its metadata to a file.

```xml
<directive name="load_and_summarize" version="1.0.0">
  <metadata>
    <description>Load a directive by ID to inspect its structure, then write a structured summary of its name, description, permissions, and steps to an output file.</description>
    <category>test/tools/primary</category>
    <author>rye-os</author>
    <model tier="fast" id="claude-3-5-haiku-20241022">Directive inspection and summarization</model>
    <limits turns="5" tokens="2048" />
    <permissions>
      <load>
        <directive>*</directive>
      </load>
      <execute>
        <tool>rye.file-system.*</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="directive_id" type="string" required="true">
      The directive ID to load and inspect (e.g., "test/tools/file_system/write_file")
    </input>
    <input name="output_path" type="string" required="true">
      File path where the summary will be written
    </input>
  </inputs>

  <outputs>
    <success>Loaded directive {input:directive_id} and wrote summary to {input:output_path}</success>
    <failure>Failed to load directive {input:directive_id} or write summary</failure>
  </outputs>
</directive>
```

<process>
  <step name="load_directive">
    <description>Load the target directive to inspect its full structure and metadata</description>
    <load item_type="directive" item_id="{input:directive_id}" />
  </step>

  <step name="write_summary">
    <description>Write a structured summary of the directive's name, description, permissions, and steps to the output path</description>
    <execute item_type="tool" item_id="rye/file-system/fs_write">
      <param name="path" value="{input:output_path}" />
      <param name="content" value="# Directive Summary: {input:directive_id}

## Name
{input:directive_id}

## Description
(extracted from loaded directive metadata)

## Permissions
(extracted from loaded directive permissions block)

## Steps
(extracted from loaded directive process steps)
" />
      <param name="mode" value="overwrite" />
    </execute>
  </step>
</process>
