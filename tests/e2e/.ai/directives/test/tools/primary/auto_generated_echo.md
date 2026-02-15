# Auto Generated Echo Directive

DESCRIPTION: A test directive for lifecycle testing

```xml
<directive>
  <metadata>
    <name>auto_generated_echo</name>
    <version>0.1.0</version>
    <description>Test directive for lifecycle testing</description>
    <category>test/tools/primary</category>
  </metadata>
  <steps>
    <step name="echo_step">
      <tool>rye_execute</tool>
      <parameters>
        <message>Hello, Directive Lifecycle Test!</message>
      </parameters>
    </step>
  </steps>
</directive>
```

STEPS:
1) echo_step - Simple echo test
  TOOL: rye_execute
  PARAMETERS:
    message: 'Hello, Directive Lifecycle Test!'
