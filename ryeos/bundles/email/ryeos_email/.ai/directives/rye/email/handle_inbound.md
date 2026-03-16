<!-- rye:signed:2026-03-16T09:27:24Z:8691bced5af8a19f96eb9001804a0dcd0a496a475e3ee3663a4b5f813a127302:6uSymr08NxjLyuJ2DNlIq6b6MApxors6N3LzRRYNqQLmLfFj3-buV1Sysr9Yb7lTAeKZwiLTm3GEfNFQU2CsCw==:4b987fd4e40303ac -->
# Handle Inbound Email

Process an inbound email: classify it, take appropriate action based on handling rules.

```xml
<directive name="handle_inbound" version="1.0.0">
  <metadata>
    <description>Classify inbound email and route to appropriate handler — forward, auto-respond, suppress, or flag for review</description>
    <category>rye/email</category>
    <author>leo</author>
    <model tier="general" />
    <limits turns="15" tokens="100000" />
    <permissions>
      <execute>
        <tool>mcp.campaign-kiwi-remote.primary_email.*</tool>
        <tool>mcp.campaign-kiwi-remote.lead.*</tool>
        <tool>mcp.campaign-kiwi-remote.campaign_email.*</tool>
        <tool>mcp.campaign-kiwi-remote.scheduler.schedule</tool>
        <directive>rye/email/forward</directive>
        <directive>rye/email/send</directive>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="email_id" type="string" required="true">
      Inbound email ID
    </input>
    <input name="from_address" type="string" required="true">
      Sender email address
    </input>
    <input name="to_address" type="string" required="true">
      Recipient email address (our inbox)
    </input>
    <input name="subject" type="string" required="true">
      Email subject line
    </input>
    <input name="body" type="string" required="true">
      Email body text
    </input>
    <input name="thread_id" type="string" required="false">
      Thread ID if this is a reply to a previous email
    </input>
    <input name="lead_id" type="string" required="false">
      Lead ID if sender is a known lead
    </input>
  </inputs>

  <outputs>
    <output name="classification">Email classification (lead_reply_positive, lead_reply_negative, business_inquiry, spam, unsubscribe, bounce, etc.)</output>
    <output name="action_taken">What the agent did (forwarded, auto_responded, suppressed, flagged)</output>
    <output name="draft_response">Suggested response text if applicable</output>
  </outputs>
</directive>
```

<process>
  <step name="load_context">
    Load handling rules and check if this sender has prior conversation history.
    If {input:lead_id} provided, look up lead data.
    If {input:thread_id} provided, load thread context.
  </step>

  <step name="classify_email">
    Based on the email content, sender, subject, and context, classify the email.
    Consider:
    - Is sender a known lead? Check lead status.
    - Is this a reply to a previous email? Check thread_id.
    - Does subject/body indicate unsubscribe, out-of-office, bounce?
    - Is this spam or auto-generated?
    - Is this a genuine business inquiry from someone new?
  </step>

  <step name="execute_action">
    Based on classification and handling rules:

    **If forward required (lead reply, business inquiry, unknown):**
    Draft a suggested response if applicable, then forward:
    `rye_execute(item_type="directive", item_id="rye/email/forward", parameters={"email_id": "{input:email_id}", "classification": "<classification>", "lead_context": "<lead_data>", "suggested_response": "<draft>"})`

    **If auto-action (spam, out-of-office, delivery confirmation):**
    Suppress — no forwarding. Update lead status if applicable.

    **If unsubscribe:**
    Update lead status to unsubscribed.

    **If bounce:**
    Update email and domain stats.
  </step>

  <step name="update_state">
    Update active conversations and agent learnings.
  </step>

  <step name="return_result">
    Return classification, action_taken, and draft_response (if applicable).
  </step>
</process>

<success_criteria>
<criterion>Email correctly classified against handling rules</criterion>
<criterion>Appropriate action taken (forward, suppress, auto-respond, flag)</criterion>
<criterion>Lead status updated if applicable</criterion>
<criterion>Conversation state updated</criterion>
</success_criteria>
