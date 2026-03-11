<!-- rye:signed:2026-03-11T07:13:35Z:8051ba10f78ba72757633167554d5730d91ee1e41726b52a6181fac53683a6f3:dVe_W_ocoyIa_iDNmIMAGCL2EBeim7NHLEiBjHKCsXXtt-dX4IU8jwDYnGSgj6a2PiSt_fjv-421lX4UezBGAw==:4b987fd4e40303ac -->
# Child Write

Simple child directive — writes a message to a file, then reads it back to confirm.

```xml
<directive name="child_write" version="1.0.0">
  <metadata>
    <description>Write a message to a file and read it back to verify. Used as a child thread target.</description>
    <category>test/tools/file_system</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="6" tokens="16000" />
    <permissions>
      <execute><tool>rye.file-system.*</tool></execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="message" type="string" required="true">
      The message to write.
    </input>
    <input name="file_path" type="string" required="true">
      Relative file path to write to (within the project workspace).
    </input>
  </inputs>

  <outputs>
    <output name="path">Path to the written file</output>
    <output name="content">The content that was verified from reading the file back</output>
  </outputs>
</directive>
```

<process>
  <step name="write">
    Write the message "{input:message}" to `{input:file_path}`.
  </step>
  <step name="verify">
    Read `{input:file_path}` back to confirm it was written correctly.
  </step>
  <step name="return_result">
    Return the path and the verified content.
  </step>
</process>
