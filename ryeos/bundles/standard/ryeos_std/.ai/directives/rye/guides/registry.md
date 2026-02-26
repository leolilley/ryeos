<!-- rye:signed:2026-02-25T07:50:41Z:fd0b17d9113aaf0ba7a9caa69f3f636ea81cf0292c70b6aa34f251ea5e26f462:38Wl44fT15Ks8JlaNeHIth8Bsq6VzyMaHXaZVZOtbg5osbSJR4Yv3Mq2y1mEEQGWQ-BqGKawsDfAftN-99_jBQ==:9fbfabe975fa5a7f -->
<!-- rye:signed:2026-02-22T02:31:19Z:f768481974fbb67c3634faee3e470062878f237f8c2aa4fd710458d3110760fa:6eQEwFmpe3e7NrjYSkZ98yGPvKkgtrwaZO_Xbi-m7b1GIpceXMUWp4czEdSuSBVTEWxkJ9bWEym7AmPKuuzXDQ==:9fbfabe975fa5a7f -->
# Registry

Guide 4 in the Rye OS onboarding sequence. Walks through the registry — pushing, pulling, publishing, searching, and the trust model.

```xml
<directive name="registry" version="1.0.0">
  <metadata>
    <description>Guide 4 in the Rye OS onboarding sequence. Walks through the registry — pushing, pulling, publishing, searching, and the trust model.</description>
    <category>rye/guides</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="15" tokens="8192" />
    <permissions>
      <execute>
        <tool>rye.core.registry.*</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs />

  <outputs>
    <output name="understanding">Understanding of registry workflow — push, pull, publish, search, and TOFU trust model</output>
  </outputs>
</directive>
```

<process>
  <step name="intro">
    <render>
The registry is where Rye items go public. Push your tools, directives, and knowledge entries. Pull from others. It's completely optional — Rye works fully without it. But if you want to share with the community or pull patterns from others, this is how.

Want to set up an account? Say yes. Want to skip? Say skip and I'll walk you through the concepts without signing up.
</render>
    <instruction>
Wait for the user to respond. If they say "skip", "no", or otherwise decline, jump directly to the "concepts" step. If they say "yes" or otherwise confirm, proceed to the "auth" step.
</instruction>
  </step>

  <step name="auth">
    <instruction>Walk through signup or login.</instruction>
    <render>
Two ways in:
</render>
    <instruction>
Present both options to the user:

1. **Email signup** — create an account with email and password:
   <tool_call>rye_execute(item_type="tool", item_id="rye/core/registry/registry", parameters={"action": "signup", "email": "you@example.com", "password": "your-password"})</tool_call>

2. **OAuth login** — opens your browser for device auth (works with GitHub):
   <tool_call>rye_execute(item_type="tool", item_id="rye/core/registry/registry", parameters={"action": "login"})</tool_call>
</instruction>
    <render>
Login opens your browser for device auth — works with GitHub OAuth. Once authenticated, check who you are:
</render>
    <tool_call>rye_execute(item_type="tool", item_id="rye/core/registry/registry", parameters={"action": "whoami"})</tool_call>
    <instruction>
Ask the user which method they prefer. Execute the appropriate one. If signup, ask for their email and password before executing.
</instruction>
  </step>

  <step name="concepts">
    <render>
The registry workflow is three operations:

**Push** — upload a local item to the registry
```
rye_execute(item_type="tool", item_id="rye/core/registry/registry",
  parameters={"action": "push", "item_type": "tool", "item_id": "your-namespace/category/name"})
```

**Pull** — download an item from the registry to your local space
```
rye_execute(item_type="tool", item_id="rye/core/registry/registry",
  parameters={"action": "pull", "item_type": "tool", "item_id": "author/category/name"})
```

**Publish** — make a pushed item publicly visible
```
rye_execute(item_type="tool", item_id="rye/core/registry/registry",
  parameters={"action": "publish", "item_type": "tool", "item_id": "your-namespace/category/name"})
```

Items start private. Push first, publish when ready.
</render>
  </step>

  <step name="search_registry">
    <render>
Search the registry with rye_search using space="registry":
</render>
    <tool_call>rye_search(scope="tool", query="*", space="registry")</tool_call>
    <instruction>
Execute the search and show the results to the user.
</instruction>
  </step>

  <step name="trust_model">
    <render>
When you pull an item for the first time, something important happens.

The registry's public key gets pinned locally — stored at `~/.ai/trusted_keys/registry.pem`. This is Trust On First Use (TOFU), the same model SSH uses for known_hosts. After pinning, every subsequent pull is verified against this key. If the registry key changes, verification fails. No silent replacement.

Pulled items also get a provenance suffix on their signature:

```
# rye:signed:TIMESTAMP:HASH:SIG:FP|rye-registry@username
```

This tells you exactly who published it. The `|rye-registry@username` suffix is proof the registry verified the author's identity.
</render>
  </step>

  <step name="next">
    <render>
That's the registry — push, pull, publish, search. TOFU key pinning for trust. Provenance tracking for accountability.

Next — multi-file tools and the anchor system:

```
rye execute directive advanced_tools
```
</render>
  </step>
</process>

<success_criteria>
<criterion>User understands the registry workflow: push, pull, publish</criterion>
<criterion>User understands how to search the registry</criterion>
<criterion>User understands the TOFU trust model and provenance signatures</criterion>
</success_criteria>
