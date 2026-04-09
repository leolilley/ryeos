<!-- rye:signed:2026-04-09T00:09:13Z:9e8f188e4169e6b71031788b2991476a988a6d69255ee195394d02807e4b8b83:gec07NSx67sMMvh9XL2TqQf_2WLfbiTBxxHTaQ6RLklphPxV73ak4TQOdBNZ_7LzRnj3zCvxRim1GaV7fdj_BA:4b987fd4e40303ac -->
# Daily Digest

Generate a summary of email activity and send it.

```xml
<directive name="daily_digest" version="1.0.0">
  <metadata>
    <description>Generate daily summary of email activity — sends, replies, bounces, pending items</description>
    <category>rye/email</category>
    <author>leo</author>
    <model tier="general" />
    <limits turns="12" tokens="50000" />
    <permissions>
      <execute>
        <tool>mcp.campaign-kiwi-remote.primary_email.*</tool>
        <tool>mcp.campaign-kiwi-remote.domain.*</tool>
        <tool>mcp.campaign-kiwi-remote.campaign.*</tool>
        <directive>rye/email/send</directive>
      </execute>
    </permissions>
  </metadata>

  <outputs>
    <output name="digest_email_id">ID of the sent digest email</output>
    <output name="summary">Brief text summary of the digest</output>
  </outputs>
</directive>
```

<process>
  <step name="gather_stats">
    Pull today's stats:
    - Domain stats (sent_today, delivered_today, bounced per domain)
    - Campaign stats (active campaigns, emails sent, replies received)
    - Inbox stats (per-inbox sent/received counts)
    - Pending items (scheduled but not sent, drafts awaiting approval)
  </step>

  <step name="gather_conversations">
    Load active conversations.
    Identify: unanswered emails, pending drafts, conversations needing follow-up.
  </step>

  <step name="compose_digest">
    Format a clean digest email:
    - Stats overview (sent, delivered, bounced, replied today)
    - Domain health (bounce rates, complaint rates)
    - Action items (unanswered emails, pending approvals)
    - Campaign progress (if any active campaigns)
  </step>

  <step name="send_digest">
    `rye_execute(item_id="rye/email/send", parameters={"to": "leo.lml.lilley@gmail.com", "subject": "[Agent Digest] <date> — <headline_stat>", "body": "<digest_body>", "send_type": "primary", "from_inbox": "leo@agentkiwi.nz"})`
  </step>

  <step name="return_result">
    Return digest_email_id and summary.
  </step>
</process>

<success_criteria>
<criterion>All stats pulled from email provider</criterion>
<criterion>Action items identified and highlighted</criterion>
<criterion>Digest sent</criterion>
</success_criteria>
