<!-- rye:signed:2026-03-16T08:41:36Z:83359e779791dd373f5c0bf1a16713cdc405a7e3446f6fd327af9d93f38aa2d1:F1IJj8z1AjsADOEqUQ7CYs9NiWOgeLQAI03qrG859UCVIOYYeA36mhvsZrmMa_UkOAgNXRI8zGbvoeqrgXzgAQ==:4b987fd4e40303ac -->
# Draft Response

Generate a reply to an email thread.

```xml
<directive name="draft_response" version="1.0.0">
  <metadata>
    <description>Draft an email response using conversation context and tone guide</description>
    <category>rye/email</category>
    <author>leo</author>
    <model tier="general" />
    <limits turns="8" tokens="30000" />
    <permissions>
      <execute>
        <!-- Tools are provider-resolved via three-tier config -->
        <tool>rye/email/providers/*</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="thread_id" type="string" required="false">
      Conversation thread ID (optional when raw email context is provided)
    </input>
    <input name="reply_intent" type="string" required="false">
      What to communicate (e.g., "schedule a call", "send pricing", "politely decline")
    </input>
    <input name="lead_id" type="string" required="false">
      Lead ID for pulling lead context
    </input>
    <input name="email_body" type="string" required="false">
      Raw email body text (used when no thread_id is available)
    </input>
    <input name="email_subject" type="string" required="false">
      Raw email subject (used when no thread_id is available)
    </input>
    <input name="from_name" type="string" required="false">
      Sender address/name (used when no thread_id is available)
    </input>
  </inputs>

  <outputs>
    <output name="draft_subject">Reply subject line</output>
    <output name="draft_body">Drafted reply body</output>
    <output name="from_inbox">Recommended sending inbox</output>
    <output name="to_email">Recipient email</output>
  </outputs>
</directive>
```

<process>
  <step name="load_thread_context">
    If {input:thread_id} provided, load full thread history via the email provider's `get` action.
    If {input:thread_id} not provided, use {input:email_body}, {input:email_subject}, and {input:from_name} as context.
    If {input:lead_id} provided, load lead data.
  </step>

  <step name="compose_reply">
    Using tone guide, conversation context, and reply intent:
    - Write in appropriate tone (warm, professional)
    - If thread history is available, reference prior conversation naturally
    - If no thread exists, compose a fresh response based on the raw email content
    - If reply_intent provided, follow it; otherwise infer appropriate response
    - Keep it concise
  </step>

  <step name="return_draft">
    Return draft_subject, draft_body, from_inbox, and to_email.
  </step>
</process>

<success_criteria>
<criterion>Draft matches appropriate tone</criterion>
<criterion>Thread context referenced naturally</criterion>
<criterion>Reply intent addressed if provided</criterion>
</success_criteria>
