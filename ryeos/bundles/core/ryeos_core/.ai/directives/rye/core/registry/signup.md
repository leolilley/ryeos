<!-- rye:signed:2026-02-26T05:52:23Z:a570264aa44efb5985d8c675d0dce81416f7b51a268595c53bcb25e965622c54:1fi9IaQjK7S5JLO21OsvxREdcvDVMy0c6F7PxJlmXrJkYC-g-teerPPI1RpNb-YzEQuMlgtiVvwBGjZnvDL8CQ==:4b987fd4e40303ac -->
# Registry Signup

Create a new registry account.

```xml
<directive name="signup" version="1.0.0">
  <metadata>
    <description>Wraps the registry tool action=signup to create a new account.</description>
    <category>rye/core/registry</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="3" tokens="2048" />
    <permissions>
      <execute>
        <tool>rye.core.registry.*</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="email" type="string" required="true">
      Email address for the new account
    </input>
    <input name="password" type="string" required="true">
      Password for the new account
    </input>
  </inputs>

  <outputs>
    <output name="account">Created account details</output>
  </outputs>
</directive>
```

<process>
  <step name="validate_inputs">
    Validate that {input:email} and {input:password} are non-empty.
  </step>

  <step name="call_registry_signup">
    Call the registry tool with action=signup.
    `rye_execute(item_type="tool", item_id="rye/core/registry/registry", parameters={"action": "signup", "email": "{input:email}", "password": "{input:password}"})`
  </step>

  <step name="return_result">
    Return the account creation result to the user.
  </step>
</process>
