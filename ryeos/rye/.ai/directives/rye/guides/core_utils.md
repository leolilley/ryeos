<!-- rye:signed:2026-02-22T02:31:19Z:8ebc16197b8fe2d13d19bc007669a69406c9baccaf8fa91c2b9f062a7d09b8ad:AT3l6SyWBKx8SdSkBv6PwlW_gxhwoqDRFKYlQJiYuihsvWxyB6-L4dx9XA29u6WI7cH1u9gtI5YilPZB0kGWCw==:9fbfabe975fa5a7f -->
# Core Utils

Guide 3 in the Rye OS onboarding sequence. Walks through the infrastructure layer — system info, telemetry, bundling, runtimes, parsers, extractors, and sinks.

```xml
<directive name="core_utils" version="1.0.0">
  <metadata>
    <description>Onboarding guide 3: Core utilities — system info, telemetry, bundling, runtimes, and infrastructure plumbing.</description>
    <category>rye/guides</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits max_turns="15" max_tokens="8192" />
    <permissions>
      <execute>
        <tool>rye.core.system.*</tool>
        <tool>rye.core.telemetry.*</tool>
        <tool>rye.core.bundler.*</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs />

  <outputs>
    <output name="understanding">Understanding of core infrastructure tools — system, telemetry, bundler, runtimes, parsers, extractors, sinks</output>
  </outputs>
</directive>
```

<process>
  <step name="intro">
    <render>
Under every directive you run and every tool you call, there's infrastructure. System info, logging, bundling, runtimes. These are the core utilities — you don't use them every day, but they're what makes everything else work. Let's look at each one.
</render>
    <instruction>Output ONLY the text in the render block above. Then immediately proceed to the next step — do NOT wait for user input.</instruction>
  </step>

  <step name="system_tool">
    <instruction>Execute the system tool to show paths, then runtime info.</instruction>
    <tool_call>rye_execute(item_type="tool", item_id="rye/core/system/system", parameters={"item": "paths"})</tool_call>
    <render>
Explain the output — project_path, user_space, system_spaces. This is the same tool the init directive used to find your user space.
</render>
    <tool_call>rye_execute(item_type="tool", item_id="rye/core/system/system", parameters={"item": "runtime"})</tool_call>
    <render>
Platform, Python version, working directory. Useful for debugging.
</render>
  </step>

  <step name="telemetry_tool">
    <instruction>Execute telemetry to show logs.</instruction>
    <tool_call>rye_execute(item_type="tool", item_id="rye/core/telemetry/telemetry", parameters={"item": "logs", "limit": 10})</tool_call>
    <render>
Every MCP call, every tool execution, every error — logged. Set `RYE_LOG_LEVEL=DEBUG` for verbose output. Use `{"item": "errors"}` to see just errors, `{"item": "stats"}` for execution statistics.
</render>
  </step>

  <step name="bundler_tool">
    <instruction>List installed bundles.</instruction>
    <tool_call>rye_execute(item_type="tool", item_id="rye/core/bundler/bundler", parameters={"action": "list"})</tool_call>
    <render>
Bundles are packages of directives, tools, and knowledge with signed manifests. The rye-os bundle is what you've been using.
</render>
    <tool_call>rye_execute(item_type="tool", item_id="rye/core/bundler/bundler", parameters={"action": "inspect", "bundle_id": "rye-os"})</tool_call>
    <render>
This shows every file in the bundle with its SHA256 hash. Both the manifest and each file have signatures — dual protection. You can create your own bundles to package and distribute your tools.
</render>
  </step>

  <step name="runtimes">
    <render>
Now the invisible layer. Runtimes sit between your tools and the execution primitives. You never call them directly, but every tool depends on one.

Available runtimes:

| Runtime | Language | What it does |
|---------|----------|-------------|
| `python_script_runtime` | Python | Subprocess with venv resolution |
| `python_function_runtime` | Python | In-process execution (faster) |
| `bash_runtime` | Bash | Shell script execution |
| `node_runtime` | Node.js | JavaScript execution |
| `state_graph_runtime` | YAML | Graph walking engine |
| `mcp_http_runtime` | HTTP | MCP protocol over HTTP |
| `mcp_stdio_runtime` | stdio | MCP protocol over stdin/stdout |

Each runtime is a YAML file in `.ai/tools/rye/core/runtimes/`. Adding a new language to Rye is creating a YAML file that tells the subprocess primitive how to invoke it — interpreter path, command template, timeout. That's it. No framework changes, no PRs. A file.

This is the data-driven philosophy. Every capability is a file. Every file is signed. The system extends by addition, not modification.
</render>
    <instruction>Output the render block above exactly as written, preserving the markdown table formatting.</instruction>
  </step>

  <step name="infrastructure">
    <render>
Three more categories you should know exist:

- **Parsers** (`rye/core/parsers/`) — Parse directive XML, extract metadata, validate structure
- **Extractors** (`rye/core/extractors/`) — Extract tool metadata (__version__, __executor_id__, etc.)
- **Sinks** (`rye/core/sinks/`) — Write output (transcript JSONL, knowledge markdown)

You won't call these directly. They're the plumbing. But if you ever need to understand how Rye processes a directive or streams output from a thread, this is where to look.
</render>
    <instruction>Output the render block above exactly as written.</instruction>
  </step>

  <step name="next">
    <render>
That's the infrastructure layer. System info, telemetry, bundles, runtimes, parsers, extractors, sinks — all files, all signed, all extensible.

Next — the registry. Sharing tools with the world:

```
rye execute directive registry
```
</render>
    <instruction>Output the render block above exactly as written. The code block contains a command the user must copy — it must appear verbatim.</instruction>
  </step>
</process>

<success_criteria>
<criterion>System tool executed with paths and runtime items, output explained</criterion>
<criterion>Telemetry logs displayed and log configuration explained</criterion>
<criterion>Bundle list and rye-os bundle inspection shown</criterion>
<criterion>Runtime table presented with data-driven philosophy explained</criterion>
<criterion>Parsers, extractors, and sinks introduced</criterion>
<criterion>User directed to the registry guide as next step</criterion>
</success_criteria>
