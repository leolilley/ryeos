<!-- rye:signed:2026-03-11T07:13:35Z:61376db74910d0e5f571813fc63a470063411c8569e9fc61889ed6cf0e20c709:zTmtj8n-_hjF7R-PdyTwn0m_fBRJnpcvJDbi5IssxY4ByVhqQGCeIpjLaYi3JIBJ1HAYn4D5YhDn_FcE5gBdAA==:4b987fd4e40303ac -->
# Hook-Routed Base Context

A context base that directives get routed into via resolve_extends hooks.
This is NOT meant to be executed directly — it's a context provider for the extends chain.

```xml
<directive name="hook_routed_base" version="1.0.0">
  <metadata>
    <description>Context base for hook-routed directives. Provides identity and rules.</description>
    <category>test/context</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="4" tokens="4096" spend="0.10" />
    <context>
      <system>test/context/base-identity</system>
      <before>test/context/hook-routed-rules</before>
    </context>
    <permissions>
      <execute><tool>rye.file-system.*</tool></execute>
      <fetch>*</fetch>
    </permissions>
  </metadata>
</directive>
```
