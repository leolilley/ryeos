<!-- rye:signed:2026-02-25T07:50:41Z:ea9c674c2c0a6527151770b4302cb206bbca50974ed2f2eae425dbf7fd2b14e1:L2LeQXTdBMSVZUxBwc9CcvZVLoJ3Jx50lL6azOr6lXIa2x2MEdWTHudiN-Af6XhFcOEIcfy2DDzGHjaAx_JHAA==:9fbfabe975fa5a7f -->
<!-- rye:signed:2026-02-22T02:31:19Z:d2124cffe0da5e985d642d3ae773010d4d8665f32f3515914485e43db5bc5424:YRudlVJKJv4e1xYJnM7-EaoYe7wBFlP0aqZAcEoFYpIz45UPDpgBhfhueQb7bKzOZzKloDrk0KpEpUP5HvYsDA==:9fbfabe975fa5a7f -->
# MCP Discovery

Guide 6 in the Rye OS onboarding sequence. Teaches MCP client integration — discover, manage, and connect to external MCP servers.

```xml
<directive name="mcp_discovery" version="1.0.0">
  <metadata>
    <description>Guide 6 in the Rye OS onboarding sequence. Teaches MCP client integration — discover, manage, and connect to external MCP servers.</description>
    <category>rye/guides</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="12" tokens="8192" />
    <permissions>
      <execute>
        <tool>rye.mcp.*</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs />

  <outputs>
    <output name="understanding">Understanding of MCP client integration — discover, manager, and connect tools</output>
  </outputs>
</directive>
```

<process>
  <step name="intro">
    <render>
This MCP is a meta MCP.

Rye OS is an MCP server — that's how your LLM talks to it. But Rye can also act as an MCP *client*, discovering and calling tools on other MCP servers. Hub and spoke. Rye at the center, external servers as spokes.

Three tools make this work: **discover**, **manager**, and **connect**.
</render>
    <instruction>Output the render block above exactly. Then proceed to the next step without waiting for user input.</instruction>
  </step>

  <step name="discover">
    <render>
The discover tool probes an MCP server and returns its available tools with full schemas.
</render>
    <instruction>Show examples for both transports.</instruction>
    <render>
**HTTP transport** — for remote servers:
```
rye_execute(item_type="tool", item_id="rye/mcp/discover",
  parameters={"transport": "http", "url": "https://example.com/mcp"})
```

**stdio transport** — for local CLI tools:
```
rye_execute(item_type="tool", item_id="rye/mcp/discover",
  parameters={"transport": "stdio", "command": "npx", "args": ["-y", "@example/mcp-server"]})
```

Both return the same thing — a list of tools with names, descriptions, and input schemas. The transport is just how the connection happens.
</render>
    <instruction>If the user has a specific MCP server they want to try, help them discover it. Otherwise, explain the output format and move on.</instruction>
  </step>

  <step name="manager">
    <render>
Once you've discovered a server, register it with the manager. This saves the connection config so you don't have to specify transport details every time.
</render>
    <tool_call>rye_execute(item_type="tool", item_id="rye/mcp/manager", parameters={"action": "add", "name": "my-server", "transport": "http", "url": "https://example.com/mcp"})</tool_call>
    <render>
Server configs are stored as YAML in `.ai/tools/mcp/servers/`. The manager also auto-discovers the server's tools on add.

Other manager actions:
- `{"action": "list"}` — list registered servers
- `{"action": "refresh", "name": "my-server"}` — re-discover tools (if the server updated)
- `{"action": "remove", "name": "my-server"}` — delete the server config
</render>
    <instruction>Output the render blocks and tool_call example above. Then proceed to the next step.</instruction>
  </step>

  <step name="connect">
    <render>
The connect tool executes a tool call on a registered server.
</render>
    <render>
**Server config mode** (recommended — uses saved config):
```
rye_execute(item_type="tool", item_id="rye/mcp/connect",
  parameters={
    "server_config": ".ai/tools/mcp/servers/my-server.yaml",
    "tool": "tool-name",
    "params": {"key": "value"}
  })
```

**Direct mode** (no saved config needed):
```
rye_execute(item_type="tool", item_id="rye/mcp/connect",
  parameters={
    "transport": "http",
    "url": "https://example.com/mcp",
    "tool": "tool-name",
    "params": {"key": "value"}
  })
```

Server config mode reads connection details from the YAML file. Direct mode specifies everything inline. Both route to the same MCP SDK call.
</render>
    <instruction>Output both render blocks above exactly. Then proceed to the next step.</instruction>
  </step>

  <step name="recursive">
    <render>
Here's where it gets interesting.

Rye is an MCP server. Rye can call other MCP servers. Those other MCP servers could be... other Rye instances. Or they could host tools that themselves call MCPs.

This creates a recursive architecture. An MCP server that calls MCP servers that call MCP servers. Each layer can have its own tools, its own permissions, its own signed items.

We're not going deep on this here — the threading guide is where recursive orchestration becomes real. But understand the shape: Rye doesn't just serve tools. It consumes them. And anything it consumes, it can orchestrate.
</render>
    <instruction>Output the render block above exactly. Then proceed to the next step.</instruction>
  </step>

  <step name="next">
    <render>
Discover, manage, connect. Three tools to integrate any MCP server into your Rye workflow.

Next — where things get truly interesting. Threads:

```
rye execute directive threading
```
</render>
    <instruction>Output the render block above exactly and stop.</instruction>
  </step>
</process>

<success_criteria>
  <criterion>User understands the discover tool and both transport modes (HTTP and stdio)</criterion>
  <criterion>User understands the manager tool for registering, listing, refreshing, and removing MCP server configs</criterion>
  <criterion>User understands the connect tool in both server config and direct modes</criterion>
  <criterion>User understands the recursive MCP architecture concept</criterion>
</success_criteria>
