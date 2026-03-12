<!-- rye:signed:2026-03-12T05:52:01Z:942e91a6709ba669d45f61019194589cd79da41feb878b6381cb98a524d341ea:9xPFCTVl7V8aI5qf2X8Stjvpq4AvbU2rN-z0Qiz4Cn2OdswEUbsekX-S1Ybw8gqltnowEjv7gksR3JCzGMRZCA==:4b987fd4e40303ac -->
# Email Send

Send an email via the ryeos email agent.

```xml
<directive name="send" version="1.0.0">
  <metadata>
    <description>Send an email. Creates the email via Campaign Kiwi and immediately approves it for delivery.</description>
    <category>email</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="6" tokens="4096" />
    <permissions>
      <execute>
        <mcp>campaign_kiwi</mcp>
        <mcp>campaign_kiwi_remote</mcp>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="to" type="string" required="true">
      Recipient email address (or comma-separated list for multiple recipients).
    </input>
    <input name="subject" type="string" required="true">
      Email subject line.
    </input>
    <input name="body" type="string" required="true">
      Email body content. Plain text — HTML will be auto-generated if needed.
    </input>
    <input name="from_inbox" type="string" required="false">
      Sending identity (email address) to send from. If omitted, uses the default configured sender.
    </input>
  </inputs>

  <outputs>
    <output name="email_id">The ID of the sent email</output>
    <output name="status">Delivery status (approved/sent)</output>
  </outputs>
</directive>
```

<process>
  <step name="validate_inputs">
    Validate that {input:to}, {input:subject}, and {input:body} are all non-empty.
    If any are missing, halt with a clear error indicating which parameter is required.
  </step>

  <step name="create_email">
    Create the email draft via Campaign Kiwi MCP:

    ```
    mcp__campaign_kiwi__execute(
      type="primary_email",
      action="create",
      params={
        "to_emails": "{input:to}",
        "subject": "{input:subject}",
        "body_text": "{input:body}",
        "from_email": "{input:from_inbox}"
      }
    )
    ```

    If {input:from_inbox} was not provided, omit `from_email` from the params.

    Capture the returned `entity_id` (or `email_id`) from the response — this is needed for the approve step.
  </step>

  <step name="approve_email">
    Approve the email for immediate delivery:

    ```
    mcp__campaign_kiwi__execute(
      type="primary_email",
      action="approve",
      params={
        "entity_id": "{email_id from previous step}"
      }
    )
    ```
  </step>

  <step name="return_result">
    Return the email ID as {output:email_id} and the delivery status as {output:status}.
    Confirm to the caller: "Email sent to {input:to} — subject: {input:subject}"
  </step>
</process>
