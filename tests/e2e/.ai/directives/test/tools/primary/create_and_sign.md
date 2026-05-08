<!-- ryeos:signed:2026-03-11T07:14:55Z:7a25e241cc82ad50e403efe1d7e2eb08045eb031cfe14f999f36ad8d7529f82a:7U41QbkoUlf6r-gXk-TktjXpYdlSHMqhXc6RbdMg_rDflzRjyDWXbeovn2CcyVoIlTs5KgX9hkWFJY9IuurwCQ==:4b987fd4e40303ac -->

# Create and Sign Knowledge Entry

Create a new knowledge entry file with YAML frontmatter, then sign it for validation.

```xml
<directive name="create_and_sign" version="1.0.0">
  <metadata>
    <description>Create a new knowledge entry file with YAML frontmatter and markdown body, then validate and sign it.</description>
    <category>test/tools/primary</category>
    <author>rye-os</author>
    <model tier="fast" id="claude-3-5-haiku-20241022">Knowledge creation and signing</model>
    <limits turns="5" tokens="2048" />
    <permissions>
      <execute>
        <tool>rye.file-system.*</tool>
      </execute>
      <sign>
        <knowledge>*</knowledge>
      </sign>
    </permissions>
  </metadata>

  <inputs>
    <input name="entry_id" type="string" required="true">
      Unique identifier for the knowledge entry in kebab-case (e.g., "caching-patterns")
    </input>
    <input name="title" type="string" required="true">
      Human-readable title for the knowledge entry
    </input>
    <input name="content" type="string" required="true">
      Markdown content for the body of the knowledge entry
    </input>
  </inputs>

  <outputs>
    <success>Created and signed knowledge entry: {input:entry_id} at .ai/knowledge/{input:entry_id}.md</success>
    <failure>Failed to create or sign knowledge entry: {input:entry_id}</failure>
  </outputs>
</directive>
```

<process>
  <step name="write_entry">
    Create the knowledge entry file at `.ai/knowledge/{input:entry_id}.md` with YAML frontmatter containing id, title, version, and the provided content as markdown body.
  </step>
  <step name="sign_entry">
    Sign the newly created knowledge entry `{input:entry_id}`.
  </step>
</process>
