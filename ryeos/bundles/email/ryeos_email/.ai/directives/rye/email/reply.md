<!-- rye:signed:2026-03-16T11:23:58Z:293513316872f6533a1abff00314e9c6dcd3838269e840e9894470cda80d8ec5:c_qzBh55w1RBuv3Tp_xJkC36Tk1Hs33sUdDZ7yBksmUT4ggbX3OzSWFgPMapMc9c11CqygdI2V6Wj-YowPa1Cw==:4b987fd4e40303ac -->
# Process Reply

When a reply is received to a forwarded email, parse the response and send it through the correct domain/inbox.

```xml
<directive name="reply" version="1.0.0">
  <metadata>
    <description>Parse a reply to a forwarded email and send through the correct domain/inbox</description>
    <category>rye/email</category>
    <author>leo</author>
    <model tier="general" />
    <limits turns="10" tokens="50000" />
    <permissions>
      <execute>
        <tool>mcp.campaign-kiwi-remote.primary_email.*</tool>
        <tool>mcp.campaign-kiwi-remote.scheduler.schedule</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="email_id" type="string" required="true">
      Inbound email ID (reply to the agent's forward)
    </input>
    <input name="from_address" type="string" required="true">
      Sender email address
    </input>
    <input name="body" type="string" required="true">
      Reply body
    </input>
    <input name="subject" type="string" required="true">
      Reply subject (contains agent metadata in prefix)
    </input>
    <input name="in_reply_to" type="string" required="false">
      Message-ID of the forwarded email
    </input>
  </inputs>

  <outputs>
    <output name="sent_email_id">ID of the email sent to the original recipient</output>
    <output name="sent_to">Recipient the reply was sent to</output>
    <output name="sent_from">Inbox the reply was sent from</output>
  </outputs>
</directive>
```

<process>
  <step name="parse_reply">
    Extract the actual reply text from the email body — strip quoted content,
    forwarding headers, and agent notes. Identify the original thread/conversation
    from the subject prefix or in_reply_to header.
  </step>

  <step name="resolve_thread">
    Look up the original conversation from the forwarded email metadata.
    Determine: original recipient, correct sending inbox, thread_id.
  </step>

  <step name="send_through_correct_domain">
    Send the reply through the original domain/inbox, properly threaded:
    `rye_execute(item_type="directive", item_id="rye/email/send", parameters={"to": "<original_sender>", "subject": "<cleaned_subject>", "body": "<reply_text>", "from_inbox": "<original_inbox>", "thread_id": "<thread_id>"})`
  </step>

  <step name="update_state">
    Update conversation state with the sent reply.
  </step>

  <step name="return_result">
    Return sent_email_id, sent_to, and sent_from.
  </step>
</process>

<success_criteria>
<criterion>Reply text extracted correctly from forwarded email</criterion>
<criterion>Reply sent through the correct domain/inbox (not the private address)</criterion>
<criterion>Reply properly threaded in the conversation</criterion>
<criterion>Conversation state updated</criterion>
</success_criteria>
