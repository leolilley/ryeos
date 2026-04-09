<!-- rye:signed:2026-04-09T00:09:13Z:cc1e9410426bcfbd01f35308906135e5002010311f9da8df704b01422054072e:J2hxQKiPKEkMGlCc_G0c4eRYxvvtHB7Zi4BCtAAeUyXpFX-jF6MAOVtwlol6V6C9Ewp9o6k1NYXHvWgRx4y-Dw:4b987fd4e40303ac -->
<!-- rye:unsigned -->
# Build With Review

Orchestrator pattern — runs a build directive, then reviews the output. Retries on rejection with feedback.

```xml
<directive name="build_with_review" version="1.0.0" extends="rye/agent/core/base">
  <metadata>
    <description>Orchestrate build → review → retry cycle with scrap-and-retry on rejection</description>
    <category>rye/code/quality</category>
    <author>rye-os</author>
    <model tier="general" />
    <limits turns="25" tokens="100000" spawns="10" />
    <context>
      <before>rye/code/quality/scrap-and-retry</before>
    </context>
    <permissions>
      <execute>
        <directive>rye.code.quality.review</directive>
        <directive>*</directive>
        <tool>rye.code.quality.gate</tool>
        <tool>rye.code.git.git</tool>
        <knowledge>*</knowledge>
      </execute>
      <fetch>*</fetch>
    </permissions>
  </metadata>

  <inputs>
    <input name="directive_id" type="string" required="true">Directive to execute as the build step</input>
    <input name="parameters" type="object" required="false">Input parameters to pass to the build directive</input>
    <input name="max_retries" type="integer" required="false">Maximum retry attempts on rejection (default: 2)</input>
  </inputs>

  <outputs>
    <output name="status">final status: approved, rejected, or error</output>
    <output name="build_thread_id">Thread ID of the final build attempt</output>
    <output name="review_verdict">The review verdict from the last review cycle</output>
    <output name="attempts">Number of build attempts made</output>
    <output name="review_reasoning">Reasoning from the final review</output>
  </outputs>
</directive>
```

<process>
  <step name="initialize">
    Set max_retries to {input:max_retries} (default: 2).
    Set attempt = 0.
    Set review_feedback = null.
  </step>

  <step name="build_loop">
    While attempt &lt; max_retries + 1:

    1. Increment attempt.

    2. Build the input parameters for the build directive.
       Start with {input:parameters} (or empty object if not provided).
       If review_feedback is not null, add it to the parameters as "review_feedback":
       this should include the previous review's verdict, reasoning, and specific issues
       so the build agent understands what to avoid.

    3. Spawn the build directive as a thread:
       `rye_execute(item_id="{input:directive_id}", parameters={"thread": "fork", ...built_params})`
       Wait for the thread to complete. Record the thread_id.

    4. Get the list of changed files from git:
       `rye_execute(item_id="rye/code/git/git", parameters={"action": "status"})`

    5. Spawn the review directive:
       `rye_execute(item_id="rye/code/quality/review", parameters={"thread": "fork", "thread_id": "<build_thread_id>", "changed_files": <changed_files>})`
       Wait for the review thread to complete.

    6. Check the review verdict:
       - If "approve": break the loop, return success.
       - If "reject": set review_feedback to the review's reasoning and issues.
         Following the scrap-and-retry principle, do NOT attempt to patch the output.
         The next iteration spawns a fresh build thread with the feedback injected.
       - If error: break the loop, return error.
  </step>

  <step name="return_results">
    Return the final status:
    - If the last review approved: status = "approved"
    - If all retries exhausted with rejections: status = "rejected"
    - Include {output:build_thread_id}, {output:review_verdict}, {output:attempts}, and {output:review_reasoning}.
  </step>
</process>
