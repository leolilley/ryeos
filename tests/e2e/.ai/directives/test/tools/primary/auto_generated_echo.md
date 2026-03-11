<!-- rye:signed:2026-03-11T07:13:35Z:43ec76e36974973999c478959a57449eff4089edd268759661d1462c22fd66f1:u-MfSlsjlYcTZgx0eM44C-EQ5qoR6u4oeXG39Y0c_9SOjXBMsPBYZ4dVKi5GYVH5gEYKmcRa7cUO7FIaE3rgBg==:4b987fd4e40303ac -->
# Auto Generated Echo Directive

A test directive for lifecycle testing — echoes a greeting message.

```xml
<directive name="auto_generated_echo" version="0.1.0">
  <metadata>
    <description>Test directive for lifecycle testing — writes an echo greeting.</description>
    <category>test/tools/primary</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="3" tokens="2048" spend="0.03" />
    <permissions>
      <execute><tool>rye.file-system.*</tool></execute>
    </permissions>
  </metadata>

  <outputs>
    <output name="message">The echoed greeting message</output>
  </outputs>
</directive>
```

<process>
  <step name="echo_step">
    Write "Hello, Directive Lifecycle Test!" to `echo_output.txt`.
  </step>
</process>
