<!-- rye:signed:2026-03-11T07:13:35Z:18f8b44fb717d21afc3cf132e9b068a03598e6b7a1b251d06a09b0cab0dd582f:u38HxRBl2U499AY5sL9Xoic1vCvRc_1qfUWz6vuVXYZRb5ntAanRPoBheevW9nVygklkS3tWk_8Joz7wClxSBQ==:4b987fd4e40303ac -->
# Directive Lifecycle Test

Test creating, signing, loading, and searching for a directive.

```xml
<directive name="directive_lifecycle_test" version="1.0.0">
  <metadata>
    <description>Test the full directive lifecycle: create, sign, load, search</description>
    <category>test/tools/primary</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="10" tokens="4096" />
    <permissions>
      <execute><tool>rye.file-system.*</tool></execute>
      <search><directive>*</directive></search>
      <load><directive>*</directive></load>
      <sign><directive>*</directive></sign>
    </permissions>
  </metadata>

  <inputs>
    <input name="test_directive_name" type="string" required="true">
      Name of the directive to create for testing.
    </input>
  </inputs>

  <outputs>
    <success>Directive lifecycle test completed for {input:test_directive_name}.</success>
  </outputs>
</directive>
```

<process>
  <step name="create_directive">
    Create a new test directive file at `.ai/directives/test/{input:test_directive_name}.md` with a valid directive markdown structure containing metadata, a simple echo step, and basic permissions.
  </step>
  <step name="sign_directive">
    Sign the newly created directive `test/{input:test_directive_name}`.
  </step>
  <step name="load_directive">
    Load the signed directive `test/{input:test_directive_name}` to verify it was created correctly.
  </step>
  <step name="search_directives">
    Search for directives matching "{input:test_directive_name}" to verify it appears in search results.
  </step>
</process>
