<!-- rye:signed:2026-03-16T11:23:58Z:b4f60f324d757eba5b321ab6e09452b5891b028b10f30cd9a482189babc15d34:x0I2dEhIDFyUifATbbftAOiE-BiLok5boL4L8C8PdbqTggZ-8HjHKKzzJrBQ_6AUPjEvg9y3hu9hUPfcV48jDg==:4b987fd4e40303ac -->
# Send Email

Send an email via the configured email provider.

```xml
<directive name="send" version="1.0.0">
  <metadata>
    <description>Send an email — create, approve, and schedule via the email provider</description>
    <category>rye/email</category>
    <author>leo</author>
    <model tier="general" />
    <limits turns="5" tokens="20000" spend="0.05" />
    <permissions>
      <execute>
        <tool>mcp.campaign-kiwi-remote.primary_email.*</tool>
        <tool>mcp.campaign-kiwi-remote.scheduler.schedule</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="to" type="string" required="true">Recipient email address</input>
    <input name="subject" type="string" required="true">Email subject line</input>
    <input name="body" type="string" required="true">Email body text</input>
    <input name="from_inbox" type="string" required="false" default="leo@agentkiwi.nz">Sending inbox</input>
    <input name="send_type" type="string" required="false" default="primary">primary or campaign</input>
    <input name="schedule_for" type="string" required="false" default="immediate">ISO 8601 or "immediate"</input>
  </inputs>

  <outputs>
    <output name="email_id">ID of the sent/scheduled email</output>
    <output name="status">sent | scheduled | error</output>
    <output name="message_id" required="false">SES message ID if sent</output>
  </outputs>
</directive>
```

<process>
  <step name="create_draft">
    Create a primary email draft with to={input:to}, from={input:from_inbox}, from_name="Leo Lilley", subject={input:subject}, body={input:body}.
  </step>

  <step name="approve_draft">
    Approve the draft using the email ID from step 1.
  </step>

  <step name="schedule_send">
    Schedule the email. Use email_type={input:send_type}, scheduled_time={input:schedule_for}.
  </step>
</process>
