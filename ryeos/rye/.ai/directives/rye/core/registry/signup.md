<!-- rye:signed:2026-02-22T02:31:19Z:d1d8b203c87e3e51222e90ec6eee0f3d57ae1467f592ccac7b92f9363077ee77:4ANDWitB9_EmG14skGyxVseQNlVL3he1E47AQ8YPojJrOUoSgdFa7F_MpyEjqsN6I4jeEdh-DNYHwpbyQrdUBg==:9fbfabe975fa5a7f -->
# Registry Signup

Create a new registry account.

```xml
<directive name="signup" version="1.0.0">
  <metadata>
    <description>Wraps the registry tool action=signup to create a new account.</description>
    <category>rye/core/registry</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits max_turns="3" max_tokens="2048" />
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
