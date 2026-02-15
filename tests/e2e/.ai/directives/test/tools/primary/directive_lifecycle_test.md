<!-- rye:signed:2026-02-12T13:40:59Z:eb3325f34e4a23c7411fd678f9df80ea7f99d8979aedebf6d0161363d36c0807:u9uuiZSRv-JGVju-ozAXhm17uQbKIbOsparVCjHitJdBcKy7gocny8LBbWEknVa4UtRrpVOKxyxTACoOr7KNCA==:440443d0858f0199 -->
# Directive Lifecycle Test

Test creating, signing, loading, and searching for a directive.

```xml
<directive name="directive_lifecycle_test" version="1.0.0">
  <metadata>
    <description>Test the full directive lifecycle: create, sign, load, search</description>
    <category>test/tools/primary</category>
    <author>rye-os</author>
    <model tier="haiku" />
    <limits turns="10" tokens="4096" />
    <permissions>
      <execute><tool>rye.file-system.*</tool></execute>
      <execute><tool>rye.primary-tools.*</tool></execute>
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

  <process>
    <step name="create_directive">
      <description>Create a new test directive file using fs_write. Write a valid directive markdown file.</description>
      <execute item_type="tool" item_id="rye/file-system/fs_write">
        <param name="path" value=".ai/directives/test/{input:test_directive_name}.md" />
        <param name="content" value="DYNAMIC" />
        <param name="mode" value="overwrite" />
      </execute>
    </step>

    <step name="sign_directive">
      <description>Sign the newly created directive.</description>
      <execute item_type="tool" item_id="rye/primary-tools/rye_sign">
        <param name="item_type" value="directive" />
        <param name="item_id" value="test/{input:test_directive_name}" />
      </execute>
    </step>

    <step name="load_directive">
      <description>Load the signed directive to verify it.</description>
      <execute item_type="tool" item_id="rye/primary-tools/rye_load">
        <param name="item_type" value="directive" />
        <param name="item_id" value="test/{input:test_directive_name}" />
      </execute>
    </step>

    <step name="search_directives">
      <description>Search for the directive by name.</description>
      <execute item_type="tool" item_id="rye/primary-tools/rye_search">
        <param name="query" value="{input:test_directive_name}" />
        <param name="item_type" value="directive" />
        <param name="limit" value="5" />
      </execute>
    </step>
  </process>

  <outputs>
    <success>Directive lifecycle test completed for {input:test_directive_name}.</success>
  </outputs>
</directive>
```
