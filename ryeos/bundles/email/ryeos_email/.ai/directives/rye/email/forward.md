<!-- rye:signed:2026-04-09T00:09:13Z:698dedad6b3397149a947787a3927bb16792e8772985a3ed045c500ae88a1cb1:cUPiC_-79y6H7nDP5AQ4oW0WXZ0Gl-MBK9NfVhsn9UdSkj477oXJiuD2kLdPUoWIid_wPfy6VoMnHls8QAjVAQ:4b987fd4e40303ac -->
# Forward Email

Forward an email to a private address with agent context and suggested response.

```xml
<directive name="forward" version="1.0.0">
  <metadata>
    <description>Forward email to a private address with classification, lead context, and suggested response</description>
    <category>rye/email</category>
    <author>leo</author>
    <model tier="general" />
    <limits turns="8" tokens="30000" />
    <permissions>
      <execute>
        <tool>mcp.campaign-kiwi-remote.primary_email.*</tool>
        <tool>mcp.campaign-kiwi-remote.scheduler.schedule</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="email_id" type="string" required="true">
      Original email ID
    </input>
    <input name="classification" type="string" required="true">
      Classification from handle_inbound
    </input>
    <input name="lead_context" type="string" required="false">
      Lead data summary (company, score, campaign, prior interaction)
    </input>
    <input name="suggested_response" type="string" required="false">
      Agent-drafted suggested reply
    </input>
  </inputs>

  <outputs>
    <output name="forwarded_email_id">ID of the forwarded email</output>
  </outputs>
</directive>
```

<process>
  <step name="fetch_original">
    Fetch the original email content.
  </step>

  <step name="build_forward_body">
    Compose the forwarded email with agent context prepended:

    ```
    === AGENT NOTES ===
    Classification: {input:classification}
    Lead: {input:lead_context}
    Reply-via: Reply to this email — your response will be routed through the agent and sent from the correct domain.

    === SUGGESTED RESPONSE ===
    {input:suggested_response}

    === ORIGINAL EMAIL ===
    From: [original sender]
    Subject: [original subject]
    [original body]
    ```
  </step>

  <step name="send_forward">
    Send the forward via `rye/email/send`:
    `rye_execute(item_id="rye/email/send", parameters={"to": "leo.lml.lilley@gmail.com", "subject": "[Agent] {input:classification}: <original_subject>", "body": "<composed_forward>", "from_inbox": "leo@agentkiwi.nz"})`
  </step>

  <step name="return_result">
    Return forwarded_email_id.
  </step>
</process>

<success_criteria>
<criterion>Email forwarded to private address with agent notes prepended</criterion>
<criterion>Classification and lead context included</criterion>
<criterion>Reply instructions included so user can respond through the agent</criterion>
</success_criteria>
