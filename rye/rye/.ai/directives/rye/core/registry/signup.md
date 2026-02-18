<!-- rye:signed:2026-02-18T05:40:31Z:70d8164af98a9fac26f56ce545b515d8ce80d567111fe8ae0d7b9e68a5d8b69d:jz5G1ZzhBXyO_hwxb4QJ_eid-vKWC1Nyg2bj6XuZ0vnrGy-Y_WZVAMyjC6Bmy2pVavHE7kyDuAVuuxeYjgbDDg==:440443d0858f0199 -->
# Registry Signup

Create a new registry account.

```xml
<directive name="signup" version="1.0.0">
  <metadata>
    <description>Wraps the registry tool action=signup to create a new account.</description>
    <category>rye/core/registry</category>
    <author>rye-os</author>
    <model tier="haiku" />
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
