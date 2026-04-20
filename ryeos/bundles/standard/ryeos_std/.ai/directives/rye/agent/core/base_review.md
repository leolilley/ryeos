<!-- rye:signed:2026-04-19T09:49:53Z:64c96297a35b53e43b5b222521c78efac6500e23ed7fdb919f8ce8f424422b41:J48En7h4vZ8uFQKOZ7OK0U4XN+KVy/JQFS6U4fLabWfGhOyYUBVWSvwFYqq+Ihz3kmmpNtBxb5U8hwtUNzymCw==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb
<!-- rye:unsigned -->

# Base Review

Operating context for review and analysis threads with read-only file access.

```xml
<directive name="base_review" version="2.0.0" extends="agent/core/base">
  <metadata>
    <description>Rye review context — extends general agent base, read-only file access</description>
    <category>rye/agent/core</category>
    <author>rye-os</author>
    <context>
      <system>rye/agent/core/Identity</system>
      <system>rye/agent/core/Behavior</system>
      <suppress>agent/core/Behavior</suppress>
    </context>
    <permissions>
      <fetch>*</fetch>
      <execute>
        <tool>rye.file-system.read</tool>
        <tool>rye.file-system.glob</tool>
        <tool>rye.file-system.grep</tool>
        <knowledge>*</knowledge>
      </execute>
    </permissions>
  </metadata>
</directive>
```
