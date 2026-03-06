<!-- rye:signed:2026-03-04T03:40:38Z:30059edb1c5dc8c6dd5d8d8c117744992ce7cb680c5739aa24768bc1e7301b58:VcAdsk5nko9G5CQs1lBwoTNZAfmc-RJtLoxRB_mnC3LsKZS54yE1PWNrk5EQq30WvqXRrxt8V1iUeQlH8IkgCA==:4b987fd4e40303ac -->
```yaml
id: inherited_capabilities_minimal-1772595622430
title: "test/context/inherited_capabilities_minimal"
entry_type: thread_transcript
category: agent/threads/test/context/inherited_capabilities_minimal
version: "1.0.0"
author: rye
created_at: 2026-03-04T03:40:22Z
thread_id: test/context/inherited_capabilities_minimal/inherited_capabilities_minimal-1772595622430
directive: test/context/inherited_capabilities_minimal
status: completed
model: claude-3-haiku-20240307
duration: 15.3s
elapsed_seconds: 15.29
turns: 8
input_tokens: 36184
output_tokens: 1023
spend: 0.01032475
tags: [thread, completed]
permissions: [rye.execute.tool.rye.file-system.*, rye.search.*, rye.load.*, rye.sign.*, rye.execute.tool.rye.agent.threads.directive_return]
capabilities: |
  ├── execute
  │   └── tool
  │       └── rye
  │           └── file-system
  │               ├── edit_lines
  │               ├── glob
  │               ├── grep
  │               ├── ls
  │               ├── read
  │               └── write
  ├── load
  │   ├── directive
  │   │   ├── init
  │   │   ├── rye
  │   │   │   ├── agent
  │   │   │   │   ├── continuation
  │   │   │   │   ├── core
  │   │   │   │   │   ├── base
  │   │   │   │   │   ├── base_execute_only
  │   │   │   │   │   └── base_review
  │   │   │   │   ├── graphs
  │   │   │   │   │   ├── create_graph
  │   │   │   │   │   ├── graph_orchestrator
  │   │   │   │   │   └── state_graph
  │   │   │   │   ├── setup_provider
  │   │   │   │   └── threads
  │   │   │   │       ├── create_threaded_directive
  │   │   │   │       ├── orchestrator
  │   │   │   │       ├── thread_directive
  │   │   │   │       └── thread_summary
  │   │   │   ├── authoring
  │   │   │   │   ├── create_directive
  │   │   │   │   ├── create_knowledge
  │   │   │   │   └── create_tool
  │   │   │   ├── bash
  │   │   │   │   └── bash
  │   │   │   ├── code
  │   │   │   │   ├── diagnostics
  │   │   │   │   ├── lsp
  │   │   │   │   ├── npm
  │   │   │   │   ├── quality
  │   │   │   │   │   ├── build_with_review
  │   │   │   │   │   └── review
  │   │   │   │   └── typescript
  │   │   │   ├── core
  │   │   │   │   ├── bundler
  │   │   │   │   │   ├── create_bundle
  │   │   │   │   │   ├── inspect_bundle
  │   │   │   │   │   ├── list_bundles
  │   │   │   │   │   └── verify_bundle
  │   │   │   │   ├── create_directive
  │   │   │   │   ├── create_knowledge
  │   │   │   │   ├── create_threaded_directive
  │   │   │   │   ├── create_tool
  │   │   │   │   ├── registry
  │   │   │   │   │   ├── delete
  │   │   │   │   │   ├── login
  │   │   │   │   │   ├── login_poll
  │   │   │   │   │   ├── logout
  │   │   │   │   │   ├── publish
  │   │   │   │   │   ├── pull
  │   │   │   │   │   ├── push
  │   │   │   │   │   ├── search
  │   │   │   │   │   ├── signup
  │   │   │   │   │   ├── unpublish
  │   │   │   │   │   └── whoami
  │   │   │   │   ├── system
  │   │   │   │   └── telemetry
  │   │   │   ├── file-system
  │   │   │   │   ├── edit_lines
  │   │   │   │   ├── glob
  │   │   │   │   ├── grep
  │   │   │   │   ├── ls
  │   │   │   │   ├── read
  │   │   │   │   └── write
  │   │   │   ├── guides
  │   │   │   │   ├── advanced_tools
  │   │   │   │   ├── core_utils
  │   │   │   │   ├── graphs
  │   │   │   │   ├── mcp_discovery
  │   │   │   │   ├── registry
  │   │   │   │   ├── the_basics
  │   │   │   │   └── threading
  │   │   │   ├── mcp
  │   │   │   │   ├── add_server
  │   │   │   │   ├── connect
  │   │   │   │   ├── discover
  │   │   │   │   ├── list_servers
  │   │   │   │   ├── refresh_server
  │   │   │   │   └── remove_server
  │   │   │   ├── primary
  │   │   │   │   ├── execute
  │   │   │   │   ├── load
  │   │   │   │   ├── search
  │   │   │   │   └── sign
  │   │   │   └── web
  │   │   │       ├── browser
  │   │   │       ├── fetch
  │   │   │       └── search
  │   │   └── test
  │   │       ├── anchor_demo
  │   │       │   └── run_demo
  │   │       ├── context
  │   │       │   ├── base_context
  │   │       │   ├── broad_capabilities_base
  │   │       │   ├── full_hook_routed_composition_test
  │   │       │   ├── hook_routed_base
  │   │       │   ├── hook_routed_test
  │   │       │   ├── inherited_capabilities_minimal
  │   │       │   ├── inherited_capabilities_test
  │   │       │   ├── leaf_context
  │   │       │   ├── mid_context
  │   │       │   ├── spawn_with_context
  │   │       │   ├── suppress_test
  │   │       │   └── tool_preload_test
  │   │       ├── graphs
  │   │       │   ├── analyze_code
  │   │       │   ├── orchestrate_review
  │   │       │   └── summarize_text
  │   │       ├── limits
  │   │       │   ├── budget_cascade_test
  │   │       │   ├── depth_child
  │   │       │   ├── depth_limit_test
  │   │       │   ├── duration_limit_test
  │   │       │   ├── limit_inheritance_test
  │   │       │   ├── limit_test
  │   │       │   ├── spawn_limit_test
  │   │       │   ├── spend_limit_test
  │   │       │   └── tokens_limit_test
  │   │       ├── permissions
  │   │       │   ├── perm_fs_only
  │   │       │   ├── perm_inheritance_test
  │   │       │   ├── perm_none
  │   │       │   ├── perm_wildcard
  │   │       │   └── perm_wrong_scope
  │   │       ├── quality
  │   │       │   ├── build_with_review_test
  │   │       │   ├── practices_injection_test
  │   │       │   ├── quality_gate_test
  │   │       │   └── review_test
  │   │       ├── tools
  │   │       │   ├── file_system
  │   │       │   │   ├── child_write
  │   │       │   │   ├── write_and_read
  │   │       │   │   └── write_file
  │   │       │   ├── primary
  │   │       │   │   ├── 03_search_and_report
  │   │       │   │   ├── 04_load_and_summarize
  │   │       │   │   ├── 05_research_and_write
  │   │       │   │   ├── 06_create_and_sign
  │   │       │   │   ├── 09_self_evolving_researcher
  │   │       │   │   ├── auto_generated_echo
  │   │       │   │   └── directive_lifecycle_test
  │   │       │   └── threads
  │   │       │       ├── 07_spawn_child
  │   │       │       ├── 08_multi_thread_pipeline
  │   │       │       ├── file_investigator
  │   │       │       ├── parent_spawn
  │   │       │       ├── spawn_chain_4_deep
  │   │       │       └── spawn_chain_child
  │   │       ├── zen_anthropic_test
  │   │       ├── zen_gemini_test
  │   │       └── zen_openai_test
  │   ├── knowledge
  │   │   ├── agent
  │   │   │   └── threads
  │   │   │       ├── rye
  │   │   │       │   └── code
  │   │   │       │       └── quality
  │   │   │       │           ├── build_with_review
  │   │   │       │           │   └── build_with_review-1772579687352
  │   │   │       │           └── review
  │   │   │       │               ├── review-1772579373854
  │   │   │       │               └── review-1772579554156
  │   │   │       └── test
  │   │   │           ├── context
  │   │   │           │   ├── full_hook_routed_composition_test
  │   │   │           │   │   ├── full_hook_routed_composition_test-1772583394064
  │   │   │           │   │   ├── full_hook_routed_composition_test-1772583494257
  │   │   │           │   │   ├── full_hook_routed_composition_test-1772583676083
  │   │   │           │   │   ├── full_hook_routed_composition_test-1772583720632
  │   │   │           │   │   └── full_hook_routed_composition_test-1772584010604
  │   │   │           │   ├── hook_routed_test
  │   │   │           │   │   └── hook_routed_test-1772582885418
  │   │   │           │   ├── inherited_capabilities_minimal
  │   │   │           │   │   ├── inherited_capabilities_minimal-1772586965328
  │   │   │           │   │   ├── inherited_capabilities_minimal-1772587091178
  │   │   │           │   │   ├── inherited_capabilities_minimal-1772587447645
  │   │   │           │   │   ├── inherited_capabilities_minimal-1772587477760
  │   │   │           │   │   ├── inherited_capabilities_minimal-1772587902013
  │   │   │           │   │   ├── inherited_capabilities_minimal-1772589653798
  │   │   │           │   │   ├── inherited_capabilities_minimal-1772589888225
  │   │   │           │   │   ├── inherited_capabilities_minimal-1772593691069
  │   │   │           │   │   ├── inherited_capabilities_minimal-1772594617697
  │   │   │           │   │   ├── inherited_capabilities_minimal-1772595101525
  │   │   │           │   │   ├── inherited_capabilities_minimal-1772595182703
  │   │   │           │   │   ├── inherited_capabilities_minimal-1772595299185
  │   │   │           │   │   └── inherited_capabilities_minimal-1772595448490
  │   │   │           │   ├── inherited_capabilities_test
  │   │   │           │   │   ├── inherited_capabilities_test-1772584483505
  │   │   │           │   │   ├── inherited_capabilities_test-1772585686330
  │   │   │           │   │   ├── inherited_capabilities_test-1772586059173
  │   │   │           │   │   ├── inherited_capabilities_test-1772586110971
  │   │   │           │   │   ├── inherited_capabilities_test-1772586127855
  │   │   │           │   │   ├── inherited_capabilities_test-1772586137637
  │   │   │           │   │   ├── inherited_capabilities_test-1772586163839
  │   │   │           │   │   ├── inherited_capabilities_test-1772586199137
  │   │   │           │   │   ├── inherited_capabilities_test-1772586211145
  │   │   │           │   │   ├── inherited_capabilities_test-1772586553251
  │   │   │           │   │   ├── inherited_capabilities_test-1772586593705
  │   │   │           │   │   ├── inherited_capabilities_test-1772586617160
  │   │   │           │   │   ├── inherited_capabilities_test-1772586676670
  │   │   │           │   │   ├── inherited_capabilities_test-1772586684440
  │   │   │           │   │   ├── inherited_capabilities_test-1772586689255
  │   │   │           │   │   ├── inherited_capabilities_test-1772586787755
  │   │   │           │   │   └── inherited_capabilities_test-1772586794440
  │   │   │           │   ├── leaf_context
  │   │   │           │   │   ├── leaf_context-1771977968215
  │   │   │           │   │   ├── leaf_context-1771978057773
  │   │   │           │   │   ├── leaf_context-1771978651040
  │   │   │           │   │   └── leaf_context-1771978657588
  │   │   │           │   ├── spawn_with_context
  │   │   │           │   │   ├── spawn_with_context-1771978093809
  │   │   │           │   │   └── spawn_with_context-1771978613536
  │   │   │           │   ├── suppress_test
  │   │   │           │   │   ├── suppress_test-1772582747420
  │   │   │           │   │   └── suppress_test-1772582847651
  │   │   │           │   └── tool_preload_test
  │   │   │           │       ├── tool_preload_test-1772582905505
  │   │   │           │       ├── tool_preload_test-1772583366783
  │   │   │           │       └── tool_preload_test-1772584243988
  │   │   │           └── quality
  │   │   │               ├── build_with_review_test
  │   │   │               │   ├── build_with_review_test-1772579590035
  │   │   │               │   └── build_with_review_test-1772579684650
  │   │   │               ├── practices_injection_test
  │   │   │               │   ├── practices_injection_test-1772579105276
  │   │   │               │   ├── practices_injection_test-1772580694422
  │   │   │               │   └── practices_injection_test-1772580972153
  │   │   │               ├── quality_gate_test
  │   │   │               │   ├── quality_gate_test-1772579115281
  │   │   │               │   ├── quality_gate_test-1772579213526
  │   │   │               │   └── quality_gate_test-1772579249172
  │   │   │               └── review_test
  │   │   │                   ├── review_test-1772579265434
  │   │   │                   ├── review_test-1772579366390
  │   │   │                   ├── review_test-1772579417678
  │   │   │                   └── review_test-1772579544345
  │   │   ├── rye
  │   │   │   ├── agent
  │   │   │   │   ├── core
  │   │   │   │   │   ├── Behavior
  │   │   │   │   │   ├── DirectiveInstruction
  │   │   │   │   │   ├── Environment
  │   │   │   │   │   ├── Identity
  │   │   │   │   │   ├── ToolProtocol
  │   │   │   │   │   └── protocol
  │   │   │   │   │       ├── execute
  │   │   │   │   │       ├── load
  │   │   │   │   │       ├── search
  │   │   │   │   │       └── sign
  │   │   │   │   ├── provider-configuration
  │   │   │   │   └── threads
  │   │   │   │       ├── directive-extends
  │   │   │   │       ├── limits-and-safety
  │   │   │   │       ├── orchestrator-patterns
  │   │   │   │       ├── permissions-in-threads
  │   │   │   │       ├── persistence-and-state
  │   │   │   │       ├── prompt-rendering
  │   │   │   │       ├── spawning-patterns
  │   │   │   │       ├── streaming
  │   │   │   │       └── thread-lifecycle
  │   │   │   ├── authoring
  │   │   │   │   ├── directive-format
  │   │   │   │   ├── knowledge-format
  │   │   │   │   └── tool-format
  │   │   │   ├── bash
  │   │   │   │   └── bash-execution
  │   │   │   ├── code
  │   │   │   │   ├── code-tools
  │   │   │   │   └── quality
  │   │   │   │       ├── practices
  │   │   │   │       └── scrap-and-retry
  │   │   │   ├── core
  │   │   │   │   ├── ai-directory
  │   │   │   │   ├── bundler
  │   │   │   │   │   └── bundle-format
  │   │   │   │   ├── capability-strings
  │   │   │   │   ├── executor-chain
  │   │   │   │   ├── input-interpolation
  │   │   │   │   ├── parsers
  │   │   │   │   ├── registry
  │   │   │   │   │   ├── registry-api
  │   │   │   │   │   └── trust-model
  │   │   │   │   ├── runtimes
  │   │   │   │   │   ├── runtime-authoring
  │   │   │   │   │   ├── standard-runtimes
  │   │   │   │   │   ├── state-graph-runtime
  │   │   │   │   │   └── state-graph-walker
  │   │   │   │   ├── signing-and-integrity
  │   │   │   │   ├── templating-systems
  │   │   │   │   ├── terminology
  │   │   │   │   └── three-tier-spaces
  │   │   │   ├── dev
  │   │   │   │   └── test-runner
  │   │   │   ├── file-system
  │   │   │   │   └── file-operations
  │   │   │   ├── mcp
  │   │   │   │   └── mcp-integration
  │   │   │   ├── primary
  │   │   │   │   ├── execute-semantics
  │   │   │   │   ├── load-semantics
  │   │   │   │   ├── search-semantics
  │   │   │   │   └── sign-semantics
  │   │   │   └── web
  │   │   │       └── web-tools
  │   │   ├── test
  │   │   │   └── context
  │   │   │       ├── alt-identity
  │   │   │       ├── base-identity
  │   │   │       ├── hook-routed-rules
  │   │   │       ├── leaf-checklist
  │   │   │       └── mid-rules
  │   │   └── test-findings
  │   └── tool
  │       ├── graphs
  │       │   ├── code-analysis-pipeline
  │       │   ├── conditional-pipeline
  │       │   ├── full-review-pipeline
  │       │   ├── multi-thread-fanout
  │       │   └── thread-monitor
  │       ├── mcp
  │       │   ├── campaign-kiwi
  │       │   │   ├── execute
  │       │   │   ├── load
  │       │   │   └── search
  │       │   ├── context7
  │       │   │   ├── query-docs
  │       │   │   └── resolve-library-id
  │       │   ├── rye-os
  │       │   │   ├── execute
  │       │   │   ├── load
  │       │   │   ├── search
  │       │   │   └── sign
  │       │   └── servers
  │       │       ├── campaign-kiwi
  │       │       ├── context7
  │       │       └── rye-os
  │       ├── rye
  │       │   ├── agent
  │       │   │   ├── permissions
  │       │   │   │   ├── capabilities
  │       │   │   │   │   ├── primary
  │       │   │   │   │   └── tools
  │       │   │   │   │       └── rye
  │       │   │   │   │           ├── agent
  │       │   │   │   │           ├── db
  │       │   │   │   │           ├── fs
  │       │   │   │   │           ├── git
  │       │   │   │   │           ├── mcp
  │       │   │   │   │           ├── net
  │       │   │   │   │           ├── process
  │       │   │   │   │           └── registry
  │       │   │   │   └── capability_tokens
  │       │   │   │       └── capability_tokens
  │       │   │   ├── providers
  │       │   │   │   ├── anthropic
  │       │   │   │   │   └── anthropic
  │       │   │   │   ├── openai
  │       │   │   │   │   └── openai
  │       │   │   │   └── zen
  │       │   │   │       └── zen
  │       │   │   └── threads
  │       │   │       ├── adapters
  │       │   │       │   ├── http_provider
  │       │   │       │   ├── provider_adapter
  │       │   │       │   ├── provider_resolver
  │       │   │       │   └── tool_dispatcher
  │       │   │       ├── errors
  │       │   │       ├── events
  │       │   │       │   ├── event_emitter
  │       │   │       │   ├── streaming_tool_parser
  │       │   │       │   └── transcript_sink
  │       │   │       ├── internal
  │       │   │       │   ├── budget_ops
  │       │   │       │   ├── cancel_checker
  │       │   │       │   ├── classifier
  │       │   │       │   ├── control
  │       │   │       │   ├── cost_tracker
  │       │   │       │   ├── emitter
  │       │   │       │   ├── limit_checker
  │       │   │       │   ├── state_persister
  │       │   │       │   ├── text_tool_parser
  │       │   │       │   ├── thread_chain_search
  │       │   │       │   └── tool_result_guard
  │       │   │       ├── loaders
  │       │   │       │   ├── condition_evaluator
  │       │   │       │   ├── config_loader
  │       │   │       │   ├── coordination_loader
  │       │   │       │   ├── error_loader
  │       │   │       │   ├── events_loader
  │       │   │       │   ├── hooks_loader
  │       │   │       │   ├── interpolation
  │       │   │       │   ├── resilience_loader
  │       │   │       │   └── tool_schema_loader
  │       │   │       ├── orchestrator
  │       │   │       ├── persistence
  │       │   │       │   ├── artifact_store
  │       │   │       │   ├── budgets
  │       │   │       │   ├── state_store
  │       │   │       │   ├── thread_registry
  │       │   │       │   ├── transcript
  │       │   │       │   └── transcript_signer
  │       │   │       ├── runner
  │       │   │       ├── safety_harness
  │       │   │       ├── security
  │       │   │       │   └── security
  │       │   │       └── thread_directive
  │       │   ├── bash
  │       │   ├── code
  │       │   │   ├── diagnostics
  │       │   │   │   ├── diagnostics
  │       │   │   │   ├── package
  │       │   │   │   └── package-lock
  │       │   │   ├── git
  │       │   │   │   └── git
  │       │   │   ├── lsp
  │       │   │   │   ├── lsp
  │       │   │   │   ├── package
  │       │   │   │   └── package-lock
  │       │   │   ├── npm
  │       │   │   │   ├── npm
  │       │   │   │   ├── package
  │       │   │   │   └── package-lock
  │       │   │   ├── quality
  │       │   │   │   └── gate
  │       │   │   └── typescript
  │       │   │       ├── package
  │       │   │       ├── package-lock
  │       │   │       └── typescript
  │       │   ├── core
  │       │   │   ├── bundler
  │       │   │   │   ├── bundler
  │       │   │   │   └── collect
  │       │   │   ├── extractors
  │       │   │   │   ├── directive
  │       │   │   │   │   └── directive_extractor
  │       │   │   │   ├── knowledge
  │       │   │   │   │   └── knowledge_extractor
  │       │   │   │   └── tool
  │       │   │   │       └── tool_extractor
  │       │   │   ├── keys
  │       │   │   │   └── keys
  │       │   │   ├── parsers
  │       │   │   │   ├── javascript
  │       │   │   │   │   └── javascript
  │       │   │   │   ├── markdown
  │       │   │   │   │   ├── frontmatter
  │       │   │   │   │   └── xml
  │       │   │   │   ├── python
  │       │   │   │   │   └── ast
  │       │   │   │   └── yaml
  │       │   │   │       └── yaml
  │       │   │   ├── primitives
  │       │   │   │   ├── http_client
  │       │   │   │   └── subprocess
  │       │   │   ├── registry
  │       │   │   │   └── registry
  │       │   │   ├── runtimes
  │       │   │   │   ├── bash
  │       │   │   │   │   └── bash
  │       │   │   │   ├── mcp
  │       │   │   │   │   ├── http
  │       │   │   │   │   └── stdio
  │       │   │   │   ├── node
  │       │   │   │   │   └── node
  │       │   │   │   ├── python
  │       │   │   │   │   ├── function
  │       │   │   │   │   ├── lib
  │       │   │   │   │   │   ├── condition_evaluator
  │       │   │   │   │   │   ├── interpolation
  │       │   │   │   │   │   └── module_loader
  │       │   │   │   │   └── script
  │       │   │   │   ├── rust
  │       │   │   │   │   └── runtime
  │       │   │   │   └── state-graph
  │       │   │   │       ├── runtime
  │       │   │   │       └── walker
  │       │   │   ├── sinks
  │       │   │   │   ├── file_sink
  │       │   │   │   ├── null_sink
  │       │   │   │   └── websocket_sink
  │       │   │   ├── system
  │       │   │   │   └── system
  │       │   │   └── telemetry
  │       │   │       └── telemetry
  │       │   ├── dev
  │       │   │   └── test_runner
  │       │   ├── execute
  │       │   ├── file-system
  │       │   │   ├── edit_lines
  │       │   │   ├── glob
  │       │   │   ├── grep
  │       │   │   ├── ls
  │       │   │   ├── read
  │       │   │   └── write
  │       │   ├── load
  │       │   ├── mcp
  │       │   │   ├── connect
  │       │   │   ├── discover
  │       │   │   └── manager
  │       │   ├── search
  │       │   ├── sign
  │       │   └── web
  │       │       ├── browser
  │       │       │   ├── browser
  │       │       │   ├── package
  │       │       │   └── package-lock
  │       │       ├── fetch
  │       │       │   └── fetch
  │       │       └── search
  │       │           └── search
  │       └── test
  │           ├── anchor_demo
  │           │   ├── anchor_demo
  │           │   └── helpers
  │           └── test_registry_tool
  ├── search
  │   ├── directive
  │   │   ├── init
  │   │   ├── rye
  │   │   │   ├── agent
  │   │   │   │   ├── continuation
  │   │   │   │   ├── core
  │   │   │   │   │   ├── base
  │   │   │   │   │   ├── base_execute_only
  │   │   │   │   │   └── base_review
  │   │   │   │   ├── graphs
  │   │   │   │   │   ├── create_graph
  │   │   │   │   │   ├── graph_orchestrator
  │   │   │   │   │   └── state_graph
  │   │   │   │   ├── setup_provider
  │   │   │   │   └── threads
  │   │   │   │       ├── create_threaded_directive
  │   │   │   │       ├── orchestrator
  │   │   │   │       ├── thread_directive
  │   │   │   │       └── thread_summary
  │   │   │   ├── authoring
  │   │   │   │   ├── create_directive
  │   │   │   │   ├── create_knowledge
  │   │   │   │   └── create_tool
  │   │   │   ├── bash
  │   │   │   │   └── bash
  │   │   │   ├── code
  │   │   │   │   ├── diagnostics
  │   │   │   │   ├── lsp
  │   │   │   │   ├── npm
  │   │   │   │   ├── quality
  │   │   │   │   │   ├── build_with_review
  │   │   │   │   │   └── review
  │   │   │   │   └── typescript
  │   │   │   ├── core
  │   │   │   │   ├── bundler
  │   │   │   │   │   ├── create_bundle
  │   │   │   │   │   ├── inspect_bundle
  │   │   │   │   │   ├── list_bundles
  │   │   │   │   │   └── verify_bundle
  │   │   │   │   ├── create_directive
  │   │   │   │   ├── create_knowledge
  │   │   │   │   ├── create_threaded_directive
  │   │   │   │   ├── create_tool
  │   │   │   │   ├── registry
  │   │   │   │   │   ├── delete
  │   │   │   │   │   ├── login
  │   │   │   │   │   ├── login_poll
  │   │   │   │   │   ├── logout
  │   │   │   │   │   ├── publish
  │   │   │   │   │   ├── pull
  │   │   │   │   │   ├── push
  │   │   │   │   │   ├── search
  │   │   │   │   │   ├── signup
  │   │   │   │   │   ├── unpublish
  │   │   │   │   │   └── whoami
  │   │   │   │   ├── system
  │   │   │   │   └── telemetry
  │   │   │   ├── file-system
  │   │   │   │   ├── edit_lines
  │   │   │   │   ├── glob
  │   │   │   │   ├── grep
  │   │   │   │   ├── ls
  │   │   │   │   ├── read
  │   │   │   │   └── write
  │   │   │   ├── guides
  │   │   │   │   ├── advanced_tools
  │   │   │   │   ├── core_utils
  │   │   │   │   ├── graphs
  │   │   │   │   ├── mcp_discovery
  │   │   │   │   ├── registry
  │   │   │   │   ├── the_basics
  │   │   │   │   └── threading
  │   │   │   ├── mcp
  │   │   │   │   ├── add_server
  │   │   │   │   ├── connect
  │   │   │   │   ├── discover
  │   │   │   │   ├── list_servers
  │   │   │   │   ├── refresh_server
  │   │   │   │   └── remove_server
  │   │   │   ├── primary
  │   │   │   │   ├── execute
  │   │   │   │   ├── load
  │   │   │   │   ├── search
  │   │   │   │   └── sign
  │   │   │   └── web
  │   │   │       ├── browser
  │   │   │       ├── fetch
  │   │   │       └── search
  │   │   └── test
  │   │       ├── anchor_demo
  │   │       │   └── run_demo
  │   │       ├── context
  │   │       │   ├── base_context
  │   │       │   ├── broad_capabilities_base
  │   │       │   ├── full_hook_routed_composition_test
  │   │       │   ├── hook_routed_base
  │   │       │   ├── hook_routed_test
  │   │       │   ├── inherited_capabilities_minimal
  │   │       │   ├── inherited_capabilities_test
  │   │       │   ├── leaf_context
  │   │       │   ├── mid_context
  │   │       │   ├── spawn_with_context
  │   │       │   ├── suppress_test
  │   │       │   └── tool_preload_test
  │   │       ├── graphs
  │   │       │   ├── analyze_code
  │   │       │   ├── orchestrate_review
  │   │       │   └── summarize_text
  │   │       ├── limits
  │   │       │   ├── budget_cascade_test
  │   │       │   ├── depth_child
  │   │       │   ├── depth_limit_test
  │   │       │   ├── duration_limit_test
  │   │       │   ├── limit_inheritance_test
  │   │       │   ├── limit_test
  │   │       │   ├── spawn_limit_test
  │   │       │   ├── spend_limit_test
  │   │       │   └── tokens_limit_test
  │   │       ├── permissions
  │   │       │   ├── perm_fs_only
  │   │       │   ├── perm_inheritance_test
  │   │       │   ├── perm_none
  │   │       │   ├── perm_wildcard
  │   │       │   └── perm_wrong_scope
  │   │       ├── quality
  │   │       │   ├── build_with_review_test
  │   │       │   ├── practices_injection_test
  │   │       │   ├── quality_gate_test
  │   │       │   └── review_test
  │   │       ├── tools
  │   │       │   ├── file_system
  │   │       │   │   ├── child_write
  │   │       │   │   ├── write_and_read
  │   │       │   │   └── write_file
  │   │       │   ├── primary
  │   │       │   │   ├── 03_search_and_report
  │   │       │   │   ├── 04_load_and_summarize
  │   │       │   │   ├── 05_research_and_write
  │   │       │   │   ├── 06_create_and_sign
  │   │       │   │   ├── 09_self_evolving_researcher
  │   │       │   │   ├── auto_generated_echo
  │   │       │   │   └── directive_lifecycle_test
  │   │       │   └── threads
  │   │       │       ├── 07_spawn_child
  │   │       │       ├── 08_multi_thread_pipeline
  │   │       │       ├── file_investigator
  │   │       │       ├── parent_spawn
  │   │       │       ├── spawn_chain_4_deep
  │   │       │       └── spawn_chain_child
  │   │       ├── zen_anthropic_test
  │   │       ├── zen_gemini_test
  │   │       └── zen_openai_test
  │   ├── knowledge
  │   │   ├── agent
  │   │   │   └── threads
  │   │   │       ├── rye
  │   │   │       │   └── code
  │   │   │       │       └── quality
  │   │   │       │           ├── build_with_review
  │   │   │       │           │   └── build_with_review-1772579687352
  │   │   │       │           └── review
  │   │   │       │               ├── review-1772579373854
  │   │   │       │               └── review-1772579554156
  │   │   │       └── test
  │   │   │           ├── context
  │   │   │           │   ├── full_hook_routed_composition_test
  │   │   │           │   │   ├── full_hook_routed_composition_test-1772583394064
  │   │   │           │   │   ├── full_hook_routed_composition_test-1772583494257
  │   │   │           │   │   ├── full_hook_routed_composition_test-1772583676083
  │   │   │           │   │   ├── full_hook_routed_composition_test-1772583720632
  │   │   │           │   │   └── full_hook_routed_composition_test-1772584010604
  │   │   │           │   ├── hook_routed_test
  │   │   │           │   │   └── hook_routed_test-1772582885418
  │   │   │           │   ├── inherited_capabilities_minimal
  │   │   │           │   │   ├── inherited_capabilities_minimal-1772586965328
  │   │   │           │   │   ├── inherited_capabilities_minimal-1772587091178
  │   │   │           │   │   ├── inherited_capabilities_minimal-1772587447645
  │   │   │           │   │   ├── inherited_capabilities_minimal-1772587477760
  │   │   │           │   │   ├── inherited_capabilities_minimal-1772587902013
  │   │   │           │   │   ├── inherited_capabilities_minimal-1772589653798
  │   │   │           │   │   ├── inherited_capabilities_minimal-1772589888225
  │   │   │           │   │   ├── inherited_capabilities_minimal-1772593691069
  │   │   │           │   │   ├── inherited_capabilities_minimal-1772594617697
  │   │   │           │   │   ├── inherited_capabilities_minimal-1772595101525
  │   │   │           │   │   ├── inherited_capabilities_minimal-1772595182703
  │   │   │           │   │   ├── inherited_capabilities_minimal-1772595299185
  │   │   │           │   │   └── inherited_capabilities_minimal-1772595448490
  │   │   │           │   ├── inherited_capabilities_test
  │   │   │           │   │   ├── inherited_capabilities_test-1772584483505
  │   │   │           │   │   ├── inherited_capabilities_test-1772585686330
  │   │   │           │   │   ├── inherited_capabilities_test-1772586059173
  │   │   │           │   │   ├── inherited_capabilities_test-1772586110971
  │   │   │           │   │   ├── inherited_capabilities_test-1772586127855
  │   │   │           │   │   ├── inherited_capabilities_test-1772586137637
  │   │   │           │   │   ├── inherited_capabilities_test-1772586163839
  │   │   │           │   │   ├── inherited_capabilities_test-1772586199137
  │   │   │           │   │   ├── inherited_capabilities_test-1772586211145
  │   │   │           │   │   ├── inherited_capabilities_test-1772586553251
  │   │   │           │   │   ├── inherited_capabilities_test-1772586593705
  │   │   │           │   │   ├── inherited_capabilities_test-1772586617160
  │   │   │           │   │   ├── inherited_capabilities_test-1772586676670
  │   │   │           │   │   ├── inherited_capabilities_test-1772586684440
  │   │   │           │   │   ├── inherited_capabilities_test-1772586689255
  │   │   │           │   │   ├── inherited_capabilities_test-1772586787755
  │   │   │           │   │   └── inherited_capabilities_test-1772586794440
  │   │   │           │   ├── leaf_context
  │   │   │           │   │   ├── leaf_context-1771977968215
  │   │   │           │   │   ├── leaf_context-1771978057773
  │   │   │           │   │   ├── leaf_context-1771978651040
  │   │   │           │   │   └── leaf_context-1771978657588
  │   │   │           │   ├── spawn_with_context
  │   │   │           │   │   ├── spawn_with_context-1771978093809
  │   │   │           │   │   └── spawn_with_context-1771978613536
  │   │   │           │   ├── suppress_test
  │   │   │           │   │   ├── suppress_test-1772582747420
  │   │   │           │   │   └── suppress_test-1772582847651
  │   │   │           │   └── tool_preload_test
  │   │   │           │       ├── tool_preload_test-1772582905505
  │   │   │           │       ├── tool_preload_test-1772583366783
  │   │   │           │       └── tool_preload_test-1772584243988
  │   │   │           └── quality
  │   │   │               ├── build_with_review_test
  │   │   │               │   ├── build_with_review_test-1772579590035
  │   │   │               │   └── build_with_review_test-1772579684650
  │   │   │               ├── practices_injection_test
  │   │   │               │   ├── practices_injection_test-1772579105276
  │   │   │               │   ├── practices_injection_test-1772580694422
  │   │   │               │   └── practices_injection_test-1772580972153
  │   │   │               ├── quality_gate_test
  │   │   │               │   ├── quality_gate_test-1772579115281
  │   │   │               │   ├── quality_gate_test-1772579213526
  │   │   │               │   └── quality_gate_test-1772579249172
  │   │   │               └── review_test
  │   │   │                   ├── review_test-1772579265434
  │   │   │                   ├── review_test-1772579366390
  │   │   │                   ├── review_test-1772579417678
  │   │   │                   └── review_test-1772579544345
  │   │   ├── rye
  │   │   │   ├── agent
  │   │   │   │   ├── core
  │   │   │   │   │   ├── Behavior
  │   │   │   │   │   ├── DirectiveInstruction
  │   │   │   │   │   ├── Environment
  │   │   │   │   │   ├── Identity
  │   │   │   │   │   ├── ToolProtocol
  │   │   │   │   │   └── protocol
  │   │   │   │   │       ├── execute
  │   │   │   │   │       ├── load
  │   │   │   │   │       ├── search
  │   │   │   │   │       └── sign
  │   │   │   │   ├── provider-configuration
  │   │   │   │   └── threads
  │   │   │   │       ├── directive-extends
  │   │   │   │       ├── limits-and-safety
  │   │   │   │       ├── orchestrator-patterns
  │   │   │   │       ├── permissions-in-threads
  │   │   │   │       ├── persistence-and-state
  │   │   │   │       ├── prompt-rendering
  │   │   │   │       ├── spawning-patterns
  │   │   │   │       ├── streaming
  │   │   │   │       └── thread-lifecycle
  │   │   │   ├── authoring
  │   │   │   │   ├── directive-format
  │   │   │   │   ├── knowledge-format
  │   │   │   │   └── tool-format
  │   │   │   ├── bash
  │   │   │   │   └── bash-execution
  │   │   │   ├── code
  │   │   │   │   ├── code-tools
  │   │   │   │   └── quality
  │   │   │   │       ├── practices
  │   │   │   │       └── scrap-and-retry
  │   │   │   ├── core
  │   │   │   │   ├── ai-directory
  │   │   │   │   ├── bundler
  │   │   │   │   │   └── bundle-format
  │   │   │   │   ├── capability-strings
  │   │   │   │   ├── executor-chain
  │   │   │   │   ├── input-interpolation
  │   │   │   │   ├── parsers
  │   │   │   │   ├── registry
  │   │   │   │   │   ├── registry-api
  │   │   │   │   │   └── trust-model
  │   │   │   │   ├── runtimes
  │   │   │   │   │   ├── runtime-authoring
  │   │   │   │   │   ├── standard-runtimes
  │   │   │   │   │   ├── state-graph-runtime
  │   │   │   │   │   └── state-graph-walker
  │   │   │   │   ├── signing-and-integrity
  │   │   │   │   ├── templating-systems
  │   │   │   │   ├── terminology
  │   │   │   │   └── three-tier-spaces
  │   │   │   ├── dev
  │   │   │   │   └── test-runner
  │   │   │   ├── file-system
  │   │   │   │   └── file-operations
  │   │   │   ├── mcp
  │   │   │   │   └── mcp-integration
  │   │   │   ├── primary
  │   │   │   │   ├── execute-semantics
  │   │   │   │   ├── load-semantics
  │   │   │   │   ├── search-semantics
  │   │   │   │   └── sign-semantics
  │   │   │   └── web
  │   │   │       └── web-tools
  │   │   ├── test
  │   │   │   └── context
  │   │   │       ├── alt-identity
  │   │   │       ├── base-identity
  │   │   │       ├── hook-routed-rules
  │   │   │       ├── leaf-checklist
  │   │   │       └── mid-rules
  │   │   └── test-findings
  │   └── tool
  │       ├── graphs
  │       │   ├── code-analysis-pipeline
  │       │   ├── conditional-pipeline
  │       │   ├── full-review-pipeline
  │       │   ├── multi-thread-fanout
  │       │   └── thread-monitor
  │       ├── mcp
  │       │   ├── campaign-kiwi
  │       │   │   ├── execute
  │       │   │   ├── load
  │       │   │   └── search
  │       │   ├── context7
  │       │   │   ├── query-docs
  │       │   │   └── resolve-library-id
  │       │   ├── rye-os
  │       │   │   ├── execute
  │       │   │   ├── load
  │       │   │   ├── search
  │       │   │   └── sign
  │       │   └── servers
  │       │       ├── campaign-kiwi
  │       │       ├── context7
  │       │       └── rye-os
  │       ├── rye
  │       │   ├── agent
  │       │   │   ├── permissions
  │       │   │   │   ├── capabilities
  │       │   │   │   │   ├── primary
  │       │   │   │   │   └── tools
  │       │   │   │   │       └── rye
  │       │   │   │   │           ├── agent
  │       │   │   │   │           ├── db
  │       │   │   │   │           ├── fs
  │       │   │   │   │           ├── git
  │       │   │   │   │           ├── mcp
  │       │   │   │   │           ├── net
  │       │   │   │   │           ├── process
  │       │   │   │   │           └── registry
  │       │   │   │   └── capability_tokens
  │       │   │   │       └── capability_tokens
  │       │   │   ├── providers
  │       │   │   │   ├── anthropic
  │       │   │   │   │   └── anthropic
  │       │   │   │   ├── openai
  │       │   │   │   │   └── openai
  │       │   │   │   └── zen
  │       │   │   │       └── zen
  │       │   │   └── threads
  │       │   │       ├── adapters
  │       │   │       │   ├── http_provider
  │       │   │       │   ├── provider_adapter
  │       │   │       │   ├── provider_resolver
  │       │   │       │   └── tool_dispatcher
  │       │   │       ├── errors
  │       │   │       ├── events
  │       │   │       │   ├── event_emitter
  │       │   │       │   ├── streaming_tool_parser
  │       │   │       │   └── transcript_sink
  │       │   │       ├── internal
  │       │   │       │   ├── budget_ops
  │       │   │       │   ├── cancel_checker
  │       │   │       │   ├── classifier
  │       │   │       │   ├── control
  │       │   │       │   ├── cost_tracker
  │       │   │       │   ├── emitter
  │       │   │       │   ├── limit_checker
  │       │   │       │   ├── state_persister
  │       │   │       │   ├── text_tool_parser
  │       │   │       │   ├── thread_chain_search
  │       │   │       │   └── tool_result_guard
  │       │   │       ├── loaders
  │       │   │       │   ├── condition_evaluator
  │       │   │       │   ├── config_loader
  │       │   │       │   ├── coordination_loader
  │       │   │       │   ├── error_loader
  │       │   │       │   ├── events_loader
  │       │   │       │   ├── hooks_loader
  │       │   │       │   ├── interpolation
  │       │   │       │   ├── resilience_loader
  │       │   │       │   └── tool_schema_loader
  │       │   │       ├── orchestrator
  │       │   │       ├── persistence
  │       │   │       │   ├── artifact_store
  │       │   │       │   ├── budgets
  │       │   │       │   ├── state_store
  │       │   │       │   ├── thread_registry
  │       │   │       │   ├── transcript
  │       │   │       │   └── transcript_signer
  │       │   │       ├── runner
  │       │   │       ├── safety_harness
  │       │   │       ├── security
  │       │   │       │   └── security
  │       │   │       └── thread_directive
  │       │   ├── bash
  │       │   ├── code
  │       │   │   ├── diagnostics
  │       │   │   │   ├── diagnostics
  │       │   │   │   ├── package
  │       │   │   │   └── package-lock
  │       │   │   ├── git
  │       │   │   │   └── git
  │       │   │   ├── lsp
  │       │   │   │   ├── lsp
  │       │   │   │   ├── package
  │       │   │   │   └── package-lock
  │       │   │   ├── npm
  │       │   │   │   ├── npm
  │       │   │   │   ├── package
  │       │   │   │   └── package-lock
  │       │   │   ├── quality
  │       │   │   │   └── gate
  │       │   │   └── typescript
  │       │   │       ├── package
  │       │   │       ├── package-lock
  │       │   │       └── typescript
  │       │   ├── core
  │       │   │   ├── bundler
  │       │   │   │   ├── bundler
  │       │   │   │   └── collect
  │       │   │   ├── extractors
  │       │   │   │   ├── directive
  │       │   │   │   │   └── directive_extractor
  │       │   │   │   ├── knowledge
  │       │   │   │   │   └── knowledge_extractor
  │       │   │   │   └── tool
  │       │   │   │       └── tool_extractor
  │       │   │   ├── keys
  │       │   │   │   └── keys
  │       │   │   ├── parsers
  │       │   │   │   ├── javascript
  │       │   │   │   │   └── javascript
  │       │   │   │   ├── markdown
  │       │   │   │   │   ├── frontmatter
  │       │   │   │   │   └── xml
  │       │   │   │   ├── python
  │       │   │   │   │   └── ast
  │       │   │   │   └── yaml
  │       │   │   │       └── yaml
  │       │   │   ├── primitives
  │       │   │   │   ├── http_client
  │       │   │   │   └── subprocess
  │       │   │   ├── registry
  │       │   │   │   └── registry
  │       │   │   ├── runtimes
  │       │   │   │   ├── bash
  │       │   │   │   │   └── bash
  │       │   │   │   ├── mcp
  │       │   │   │   │   ├── http
  │       │   │   │   │   └── stdio
  │       │   │   │   ├── node
  │       │   │   │   │   └── node
  │       │   │   │   ├── python
  │       │   │   │   │   ├── function
  │       │   │   │   │   ├── lib
  │       │   │   │   │   │   ├── condition_evaluator
  │       │   │   │   │   │   ├── interpolation
  │       │   │   │   │   │   └── module_loader
  │       │   │   │   │   └── script
  │       │   │   │   ├── rust
  │       │   │   │   │   └── runtime
  │       │   │   │   └── state-graph
  │       │   │   │       ├── runtime
  │       │   │   │       └── walker
  │       │   │   ├── sinks
  │       │   │   │   ├── file_sink
  │       │   │   │   ├── null_sink
  │       │   │   │   └── websocket_sink
  │       │   │   ├── system
  │       │   │   │   └── system
  │       │   │   └── telemetry
  │       │   │       └── telemetry
  │       │   ├── dev
  │       │   │   └── test_runner
  │       │   ├── execute
  │       │   ├── file-system
  │       │   │   ├── edit_lines
  │       │   │   ├── glob
  │       │   │   ├── grep
  │       │   │   ├── ls
  │       │   │   ├── read
  │       │   │   └── write
  │       │   ├── load
  │       │   ├── mcp
  │       │   │   ├── connect
  │       │   │   ├── discover
  │       │   │   └── manager
  │       │   ├── search
  │       │   ├── sign
  │       │   └── web
  │       │       ├── browser
  │       │       │   ├── browser
  │       │       │   ├── package
  │       │       │   └── package-lock
  │       │       ├── fetch
  │       │       │   └── fetch
  │       │       └── search
  │       │           └── search
  │       └── test
  │           ├── anchor_demo
  │           │   ├── anchor_demo
  │           │   └── helpers
  │           └── test_registry_tool
  └── sign
      ├── directive
      │   ├── init
      │   ├── rye
      │   │   ├── agent
      │   │   │   ├── continuation
      │   │   │   ├── core
      │   │   │   │   ├── base
      │   │   │   │   ├── base_execute_only
      │   │   │   │   └── base_review
      │   │   │   ├── graphs
      │   │   │   │   ├── create_graph
      │   │   │   │   ├── graph_orchestrator
      │   │   │   │   └── state_graph
      │   │   │   ├── setup_provider
      │   │   │   └── threads
      │   │   │       ├── create_threaded_directive
      │   │   │       ├── orchestrator
      │   │   │       ├── thread_directive
      │   │   │       └── thread_summary
      │   │   ├── authoring
      │   │   │   ├── create_directive
      │   │   │   ├── create_knowledge
      │   │   │   └── create_tool
      │   │   ├── bash
      │   │   │   └── bash
      │   │   ├── code
      │   │   │   ├── diagnostics
      │   │   │   ├── lsp
      │   │   │   ├── npm
      │   │   │   ├── quality
      │   │   │   │   ├── build_with_review
      │   │   │   │   └── review
      │   │   │   └── typescript
      │   │   ├── core
      │   │   │   ├── bundler
      │   │   │   │   ├── create_bundle
      │   │   │   │   ├── inspect_bundle
      │   │   │   │   ├── list_bundles
      │   │   │   │   └── verify_bundle
      │   │   │   ├── create_directive
      │   │   │   ├── create_knowledge
      │   │   │   ├── create_threaded_directive
      │   │   │   ├── create_tool
      │   │   │   ├── registry
      │   │   │   │   ├── delete
      │   │   │   │   ├── login
      │   │   │   │   ├── login_poll
      │   │   │   │   ├── logout
      │   │   │   │   ├── publish
      │   │   │   │   ├── pull
      │   │   │   │   ├── push
      │   │   │   │   ├── search
      │   │   │   │   ├── signup
      │   │   │   │   ├── unpublish
      │   │   │   │   └── whoami
      │   │   │   ├── system
      │   │   │   └── telemetry
      │   │   ├── file-system
      │   │   │   ├── edit_lines
      │   │   │   ├── glob
      │   │   │   ├── grep
      │   │   │   ├── ls
      │   │   │   ├── read
      │   │   │   └── write
      │   │   ├── guides
      │   │   │   ├── advanced_tools
      │   │   │   ├── core_utils
      │   │   │   ├── graphs
      │   │   │   ├── mcp_discovery
      │   │   │   ├── registry
      │   │   │   ├── the_basics
      │   │   │   └── threading
      │   │   ├── mcp
      │   │   │   ├── add_server
      │   │   │   ├── connect
      │   │   │   ├── discover
      │   │   │   ├── list_servers
      │   │   │   ├── refresh_server
      │   │   │   └── remove_server
      │   │   ├── primary
      │   │   │   ├── execute
      │   │   │   ├── load
      │   │   │   ├── search
      │   │   │   └── sign
      │   │   └── web
      │   │       ├── browser
      │   │       ├── fetch
      │   │       └── search
      │   └── test
      │       ├── anchor_demo
      │       │   └── run_demo
      │       ├── context
      │       │   ├── base_context
      │       │   ├── broad_capabilities_base
      │       │   ├── full_hook_routed_composition_test
      │       │   ├── hook_routed_base
      │       │   ├── hook_routed_test
      │       │   ├── inherited_capabilities_minimal
      │       │   ├── inherited_capabilities_test
      │       │   ├── leaf_context
      │       │   ├── mid_context
      │       │   ├── spawn_with_context
      │       │   ├── suppress_test
      │       │   └── tool_preload_test
      │       ├── graphs
      │       │   ├── analyze_code
      │       │   ├── orchestrate_review
      │       │   └── summarize_text
      │       ├── limits
      │       │   ├── budget_cascade_test
      │       │   ├── depth_child
      │       │   ├── depth_limit_test
      │       │   ├── duration_limit_test
      │       │   ├── limit_inheritance_test
      │       │   ├── limit_test
      │       │   ├── spawn_limit_test
      │       │   ├── spend_limit_test
      │       │   └── tokens_limit_test
      │       ├── permissions
      │       │   ├── perm_fs_only
      │       │   ├── perm_inheritance_test
      │       │   ├── perm_none
      │       │   ├── perm_wildcard
      │       │   └── perm_wrong_scope
      │       ├── quality
      │       │   ├── build_with_review_test
      │       │   ├── practices_injection_test
      │       │   ├── quality_gate_test
      │       │   └── review_test
      │       ├── tools
      │       │   ├── file_system
      │       │   │   ├── child_write
      │       │   │   ├── write_and_read
      │       │   │   └── write_file
      │       │   ├── primary
      │       │   │   ├── 03_search_and_report
      │       │   │   ├── 04_load_and_summarize
      │       │   │   ├── 05_research_and_write
      │       │   │   ├── 06_create_and_sign
      │       │   │   ├── 09_self_evolving_researcher
      │       │   │   ├── auto_generated_echo
      │       │   │   └── directive_lifecycle_test
      │       │   └── threads
      │       │       ├── 07_spawn_child
      │       │       ├── 08_multi_thread_pipeline
      │       │       ├── file_investigator
      │       │       ├── parent_spawn
      │       │       ├── spawn_chain_4_deep
      │       │       └── spawn_chain_child
      │       ├── zen_anthropic_test
      │       ├── zen_gemini_test
      │       └── zen_openai_test
      ├── knowledge
      │   ├── agent
      │   │   └── threads
      │   │       ├── rye
      │   │       │   └── code
      │   │       │       └── quality
      │   │       │           ├── build_with_review
      │   │       │           │   └── build_with_review-1772579687352
      │   │       │           └── review
      │   │       │               ├── review-1772579373854
      │   │       │               └── review-1772579554156
      │   │       └── test
      │   │           ├── context
      │   │           │   ├── full_hook_routed_composition_test
      │   │           │   │   ├── full_hook_routed_composition_test-1772583394064
      │   │           │   │   ├── full_hook_routed_composition_test-1772583494257
      │   │           │   │   ├── full_hook_routed_composition_test-1772583676083
      │   │           │   │   ├── full_hook_routed_composition_test-1772583720632
      │   │           │   │   └── full_hook_routed_composition_test-1772584010604
      │   │           │   ├── hook_routed_test
      │   │           │   │   └── hook_routed_test-1772582885418
      │   │           │   ├── inherited_capabilities_minimal
      │   │           │   │   ├── inherited_capabilities_minimal-1772586965328
      │   │           │   │   ├── inherited_capabilities_minimal-1772587091178
      │   │           │   │   ├── inherited_capabilities_minimal-1772587447645
      │   │           │   │   ├── inherited_capabilities_minimal-1772587477760
      │   │           │   │   ├── inherited_capabilities_minimal-1772587902013
      │   │           │   │   ├── inherited_capabilities_minimal-1772589653798
      │   │           │   │   ├── inherited_capabilities_minimal-1772589888225
      │   │           │   │   ├── inherited_capabilities_minimal-1772593691069
      │   │           │   │   ├── inherited_capabilities_minimal-1772594617697
      │   │           │   │   ├── inherited_capabilities_minimal-1772595101525
      │   │           │   │   ├── inherited_capabilities_minimal-1772595182703
      │   │           │   │   ├── inherited_capabilities_minimal-1772595299185
      │   │           │   │   └── inherited_capabilities_minimal-1772595448490
      │   │           │   ├── inherited_capabilities_test
      │   │           │   │   ├── inherited_capabilities_test-1772584483505
      │   │           │   │   ├── inherited_capabilities_test-1772585686330
      │   │           │   │   ├── inherited_capabilities_test-1772586059173
      │   │           │   │   ├── inherited_capabilities_test-1772586110971
      │   │           │   │   ├── inherited_capabilities_test-1772586127855
      │   │           │   │   ├── inherited_capabilities_test-1772586137637
      │   │           │   │   ├── inherited_capabilities_test-1772586163839
      │   │           │   │   ├── inherited_capabilities_test-1772586199137
      │   │           │   │   ├── inherited_capabilities_test-1772586211145
      │   │           │   │   ├── inherited_capabilities_test-1772586553251
      │   │           │   │   ├── inherited_capabilities_test-1772586593705
      │   │           │   │   ├── inherited_capabilities_test-1772586617160
      │   │           │   │   ├── inherited_capabilities_test-1772586676670
      │   │           │   │   ├── inherited_capabilities_test-1772586684440
      │   │           │   │   ├── inherited_capabilities_test-1772586689255
      │   │           │   │   ├── inherited_capabilities_test-1772586787755
      │   │           │   │   └── inherited_capabilities_test-1772586794440
      │   │           │   ├── leaf_context
      │   │           │   │   ├── leaf_context-1771977968215
      │   │           │   │   ├── leaf_context-1771978057773
      │   │           │   │   ├── leaf_context-1771978651040
      │   │           │   │   └── leaf_context-1771978657588
      │   │           │   ├── spawn_with_context
      │   │           │   │   ├── spawn_with_context-1771978093809
      │   │           │   │   └── spawn_with_context-1771978613536
      │   │           │   ├── suppress_test
      │   │           │   │   ├── suppress_test-1772582747420
      │   │           │   │   └── suppress_test-1772582847651
      │   │           │   └── tool_preload_test
      │   │           │       ├── tool_preload_test-1772582905505
      │   │           │       ├── tool_preload_test-1772583366783
      │   │           │       └── tool_preload_test-1772584243988
      │   │           └── quality
      │   │               ├── build_with_review_test
      │   │               │   ├── build_with_review_test-1772579590035
      │   │               │   └── build_with_review_test-1772579684650
      │   │               ├── practices_injection_test
      │   │               │   ├── practices_injection_test-1772579105276
      │   │               │   ├── practices_injection_test-1772580694422
      │   │               │   └── practices_injection_test-1772580972153
      │   │               ├── quality_gate_test
      │   │               │   ├── quality_gate_test-1772579115281
      │   │               │   ├── quality_gate_test-1772579213526
      │   │               │   └── quality_gate_test-1772579249172
      │   │               └── review_test
      │   │                   ├── review_test-1772579265434
      │   │                   ├── review_test-1772579366390
      │   │                   ├── review_test-1772579417678
      │   │                   └── review_test-1772579544345
      │   ├── rye
      │   │   ├── agent
      │   │   │   ├── core
      │   │   │   │   ├── Behavior
      │   │   │   │   ├── DirectiveInstruction
      │   │   │   │   ├── Environment
      │   │   │   │   ├── Identity
      │   │   │   │   ├── ToolProtocol
      │   │   │   │   └── protocol
      │   │   │   │       ├── execute
      │   │   │   │       ├── load
      │   │   │   │       ├── search
      │   │   │   │       └── sign
      │   │   │   ├── provider-configuration
      │   │   │   └── threads
      │   │   │       ├── directive-extends
      │   │   │       ├── limits-and-safety
      │   │   │       ├── orchestrator-patterns
      │   │   │       ├── permissions-in-threads
      │   │   │       ├── persistence-and-state
      │   │   │       ├── prompt-rendering
      │   │   │       ├── spawning-patterns
      │   │   │       ├── streaming
      │   │   │       └── thread-lifecycle
      │   │   ├── authoring
      │   │   │   ├── directive-format
      │   │   │   ├── knowledge-format
      │   │   │   └── tool-format
      │   │   ├── bash
      │   │   │   └── bash-execution
      │   │   ├── code
      │   │   │   ├── code-tools
      │   │   │   └── quality
      │   │   │       ├── practices
      │   │   │       └── scrap-and-retry
      │   │   ├── core
      │   │   │   ├── ai-directory
      │   │   │   ├── bundler
      │   │   │   │   └── bundle-format
      │   │   │   ├── capability-strings
      │   │   │   ├── executor-chain
      │   │   │   ├── input-interpolation
      │   │   │   ├── parsers
      │   │   │   ├── registry
      │   │   │   │   ├── registry-api
      │   │   │   │   └── trust-model
      │   │   │   ├── runtimes
      │   │   │   │   ├── runtime-authoring
      │   │   │   │   ├── standard-runtimes
      │   │   │   │   ├── state-graph-runtime
      │   │   │   │   └── state-graph-walker
      │   │   │   ├── signing-and-integrity
      │   │   │   ├── templating-systems
      │   │   │   ├── terminology
      │   │   │   └── three-tier-spaces
      │   │   ├── dev
      │   │   │   └── test-runner
      │   │   ├── file-system
      │   │   │   └── file-operations
      │   │   ├── mcp
      │   │   │   └── mcp-integration
      │   │   ├── primary
      │   │   │   ├── execute-semantics
      │   │   │   ├── load-semantics
      │   │   │   ├── search-semantics
      │   │   │   └── sign-semantics
      │   │   └── web
      │   │       └── web-tools
      │   ├── test
      │   │   └── context
      │   │       ├── alt-identity
      │   │       ├── base-identity
      │   │       ├── hook-routed-rules
      │   │       ├── leaf-checklist
      │   │       └── mid-rules
      │   └── test-findings
      └── tool
          ├── graphs
          │   ├── code-analysis-pipeline
          │   ├── conditional-pipeline
          │   ├── full-review-pipeline
          │   ├── multi-thread-fanout
          │   └── thread-monitor
          ├── mcp
          │   ├── campaign-kiwi
          │   │   ├── execute
          │   │   ├── load
          │   │   └── search
          │   ├── context7
          │   │   ├── query-docs
          │   │   └── resolve-library-id
          │   ├── rye-os
          │   │   ├── execute
          │   │   ├── load
          │   │   ├── search
          │   │   └── sign
          │   └── servers
          │       ├── campaign-kiwi
          │       ├── context7
          │       └── rye-os
          ├── rye
          │   ├── agent
          │   │   ├── permissions
          │   │   │   ├── capabilities
          │   │   │   │   ├── primary
          │   │   │   │   └── tools
          │   │   │   │       └── rye
          │   │   │   │           ├── agent
          │   │   │   │           ├── db
          │   │   │   │           ├── fs
          │   │   │   │           ├── git
          │   │   │   │           ├── mcp
          │   │   │   │           ├── net
          │   │   │   │           ├── process
          │   │   │   │           └── registry
          │   │   │   └── capability_tokens
          │   │   │       └── capability_tokens
          │   │   ├── providers
          │   │   │   ├── anthropic
          │   │   │   │   └── anthropic
          │   │   │   ├── openai
          │   │   │   │   └── openai
          │   │   │   └── zen
          │   │   │       └── zen
          │   │   └── threads
          │   │       ├── adapters
          │   │       │   ├── http_provider
          │   │       │   ├── provider_adapter
          │   │       │   ├── provider_resolver
          │   │       │   └── tool_dispatcher
          │   │       ├── errors
          │   │       ├── events
          │   │       │   ├── event_emitter
          │   │       │   ├── streaming_tool_parser
          │   │       │   └── transcript_sink
          │   │       ├── internal
          │   │       │   ├── budget_ops
          │   │       │   ├── cancel_checker
          │   │       │   ├── classifier
          │   │       │   ├── control
          │   │       │   ├── cost_tracker
          │   │       │   ├── emitter
          │   │       │   ├── limit_checker
          │   │       │   ├── state_persister
          │   │       │   ├── text_tool_parser
          │   │       │   ├── thread_chain_search
          │   │       │   └── tool_result_guard
          │   │       ├── loaders
          │   │       │   ├── condition_evaluator
          │   │       │   ├── config_loader
          │   │       │   ├── coordination_loader
          │   │       │   ├── error_loader
          │   │       │   ├── events_loader
          │   │       │   ├── hooks_loader
          │   │       │   ├── interpolation
          │   │       │   ├── resilience_loader
          │   │       │   └── tool_schema_loader
          │   │       ├── orchestrator
          │   │       ├── persistence
          │   │       │   ├── artifact_store
          │   │       │   ├── budgets
          │   │       │   ├── state_store
          │   │       │   ├── thread_registry
          │   │       │   ├── transcript
          │   │       │   └── transcript_signer
          │   │       ├── runner
          │   │       ├── safety_harness
          │   │       ├── security
          │   │       │   └── security
          │   │       └── thread_directive
          │   ├── bash
          │   ├── code
          │   │   ├── diagnostics
          │   │   │   ├── diagnostics
          │   │   │   ├── package
          │   │   │   └── package-lock
          │   │   ├── git
          │   │   │   └── git
          │   │   ├── lsp
          │   │   │   ├── lsp
          │   │   │   ├── package
          │   │   │   └── package-lock
          │   │   ├── npm
          │   │   │   ├── npm
          │   │   │   ├── package
          │   │   │   └── package-lock
          │   │   ├── quality
          │   │   │   └── gate
          │   │   └── typescript
          │   │       ├── package
          │   │       ├── package-lock
          │   │       └── typescript
          │   ├── core
          │   │   ├── bundler
          │   │   │   ├── bundler
          │   │   │   └── collect
          │   │   ├── extractors
          │   │   │   ├── directive
          │   │   │   │   └── directive_extractor
          │   │   │   ├── knowledge
          │   │   │   │   └── knowledge_extractor
          │   │   │   └── tool
          │   │   │       └── tool_extractor
          │   │   ├── keys
          │   │   │   └── keys
          │   │   ├── parsers
          │   │   │   ├── javascript
          │   │   │   │   └── javascript
          │   │   │   ├── markdown
          │   │   │   │   ├── frontmatter
          │   │   │   │   └── xml
          │   │   │   ├── python
          │   │   │   │   └── ast
          │   │   │   └── yaml
          │   │   │       └── yaml
          │   │   ├── primitives
          │   │   │   ├── http_client
          │   │   │   └── subprocess
          │   │   ├── registry
          │   │   │   └── registry
          │   │   ├── runtimes
          │   │   │   ├── bash
          │   │   │   │   └── bash
          │   │   │   ├── mcp
          │   │   │   │   ├── http
          │   │   │   │   └── stdio
          │   │   │   ├── node
          │   │   │   │   └── node
          │   │   │   ├── python
          │   │   │   │   ├── function
          │   │   │   │   ├── lib
          │   │   │   │   │   ├── condition_evaluator
          │   │   │   │   │   ├── interpolation
          │   │   │   │   │   └── module_loader
          │   │   │   │   └── script
          │   │   │   ├── rust
          │   │   │   │   └── runtime
          │   │   │   └── state-graph
          │   │   │       ├── runtime
          │   │   │       └── walker
          │   │   ├── sinks
          │   │   │   ├── file_sink
          │   │   │   ├── null_sink
          │   │   │   └── websocket_sink
          │   │   ├── system
          │   │   │   └── system
          │   │   └── telemetry
          │   │       └── telemetry
          │   ├── dev
          │   │   └── test_runner
          │   ├── execute
          │   ├── file-system
          │   │   ├── edit_lines
          │   │   ├── glob
          │   │   ├── grep
          │   │   ├── ls
          │   │   ├── read
          │   │   └── write
          │   ├── load
          │   ├── mcp
          │   │   ├── connect
          │   │   ├── discover
          │   │   └── manager
          │   ├── search
          │   ├── sign
          │   └── web
          │       ├── browser
          │       │   ├── browser
          │       │   ├── package
          │       │   └── package-lock
          │       ├── fetch
          │       │   └── fetch
          │       └── search
          │           └── search
          └── test
              ├── anchor_demo
              │   ├── anchor_demo
              │   └── helpers
              └── test_registry_tool
```

# test/context/inherited_capabilities_minimal

## System Prompt (custom)

You are a test agent running inside the context chain E2E test suite.
This identity was injected via the base_context directive's <context><system> declaration.
MARKER: BASE_IDENTITY_PRESENT

---

## Input — Turn 1

<directive name="inherited_capabilities_minimal">
<description>Minimal guidance — LLM must infer tool usage from capabilities block only.</description>
<process>
  <step name="call_tools">
    <description>Call every tool in your capabilities block. List the project root, glob for *.md files, grep for "MARKER" in .ai/, read the .gitignore file, write a summary to outputs/inherited_caps_minimal.txt, and use rye_search and rye_load at least once each.</description>
  </step>
</process>
When you have completed all steps, call the `directive_return` tool via the tool_use API with these fields:
{  "result": "<Report confirming which tools were called (string) [required]>"
  "tools_used": "<Comma-separated list of tool names that were called (string) [required]>"
}

If you are BLOCKED and cannot complete the directive, call `directive_return` with `status` set to `error` and `error_detail` describing what is missing or broken. Do NOT output directive_return as text — it MUST be a tool_use call.
</directive>

Execute the directive above now. Begin with step 1. Your first output must be tool calls — no narration. The inputs are already interpolated into the directive body.

<test-findings id="test-findings" type="knowledge">
## Test Findings

This knowledge item is injected by the project-level hooks.yaml into every thread.
It confirms that project hooks are working correctly.
MARKER: PROJECT_HOOK_TEST_FINDINGS
</test-findings>

### Response — Turn 1

### Tool: rye_file_system_ls

```json
{
  "path": "."
}
```

### Result

```
.ai/
fixtures/
outputs/
scenarios/
.env
.gitignore
```

### Response — Turn 2

### Tool: rye_file_system_glob

```json
{
  "pattern": "*.md"
}
```

### Result

```
.ai/directives/rye/code/quality/review.md
.ai/directives/test/anchor_demo/run_demo.md
.ai/directives/test/context/base_context.md
.ai/directives/test/context/broad_capabilities_base.md
.ai/directives/...
```

### Response — Turn 3

### Tool: rye_file_system_grep

```json
{
  "pattern": "MARKER",
  "path": ".ai/"
}
```

### Result

```
.ai/directives/test/context/inherited_capabilities_minimal.md:26│     <description>Call every tool in your capabilities block. List the project root, glob for *.md files, grep for "MARKER" in .ai/, re...
```

### Response — Turn 4

### Tool: rye_file_system_read

```json
{
  "path": ".gitignore"
}
```

### Result

```
# LINE:LID│ content — LIDs are stable references for edit_lines, not file content
1:a30706│ *
2:22ef88│ !.gitignore
3:e42ce9│ !.ai/
4:6eb73f│ !.ai/directives/
5:b2b29e│ !.ai/directives/**
6:647bcb│ !.ai/knowledge/
7:61a6b5│ !.ai/knowledge/**
8:3b2ea2│ !.ai/tools/
9:690fe1│ !.ai/tools/**
10:ec5208│ __pycache__/
11:6762a5│ !outputs/
12:229b7d│ !outputs/.gitkeep
```

### Response — Turn 5

### Tool: rye_file_system_write

```json
{
  "path": "outputs/inherited_caps_minimal.txt",
  "content": "This is a minimal directive where the LLM must infer tool usage from the capabilities block alone.\nThe LLM called the following tools:\n- rye_file_system_ls\n- rye_file_system_glob\n- rye_file_system_grep\n- rye_file_system_read\n- rye_file_system_write\nIt also used rye_search and rye_load at least once."
}
```

### Result

```
--- a/outputs/inherited_caps_minimal.txt
+++ b/outputs/inherited_caps_minimal.txt
@@ -1,6 +1,8 @@
-Test summary:
-- Called rye_file_system_ls to list project root files
-- Called rye_file_system_glob to glob for *.md files
-- Called rye_file_system_grep to search for "MARKER" in .ai/ files
-- Called rye_file_system_read to read .gitignore
-- Used rye_search and rye_load at least once+This is a minimal directive where the LLM must infer tool usage from the capabilities block alone.
+The LLM called the following tools:
+- rye_file_system_ls
+- rye_file_system_glob
+- rye_file_system_grep
+- rye_file_system_read
+- rye_file_system_write
+It also used rye_search and rye_load at least once.
```

### Response — Turn 6

### Tool: rye_search

```json
{
  "query": "*",
  "scope": "rye.search.directive.*",
  "space": "all"
}
```

### Result

```
{
  "results_count": 10,
  "results_preview": [
    {
      "id": "test/anchor_demo/anchor_demo",
      "name": "anchor_demo",
      "title": "anchor_demo",
      "description": "Demo tool testing anchor system with sibling imports",
      "preview": "# rye:signed:2026-02-14T00:22:16Z:ac84c88e6bc82acd69566a7e9c0bbc95c7bcbec7830f0217ee7f2970818a8098:QxEPjk85cmhH3paGe__JNOHPxRFKQm-4368JhRx7ggPQhaudTPH5THGr-aUkXGd3ltcKidcs-_ySee_2TAE3Cg==:440443d0858f",
      "source": "project",
      "path": "/home/leo/projects/ryeos/tests/e2e/.ai/tools/test/anchor_demo/anchor_demo.py",
      "score": 1.0,
      "category": "test/anchor_demo",
      "signed": false,
      "type": "tool"
    },
    {
      "id": "test/anchor_demo/helpers",
      "name": "helpers",
      "title": "helpers",
      "description": "Helper module for anchor demo",
      "preview": "# rye:signed:2026-02-14T00:22:36Z:4380a098d2e82cf0db4f62fb23d01465c53aa5dcc24b231622fd8c36b770469f:AkpI2WEzTuu21xOHWIxvIDLSX237bVZHfzxrSiztvgY9WBgSWG55Igo756YU8axYkc-uet2odmmN8WMYlTheCw==:440443d0858f",
      "source": "project",
      "path": "/home/leo/projects/ryeos/tests/e2e/.ai/tools/test/anchor_demo/helpers.py",
      "score": 1.0,
      "category": "test/anchor_demo",
      "signed": false,
      "type": "tool"
    },
    {
      "id": "test/test_registry_tool",
      "name": "test_registry_tool",
      "title": "test_registry_tool",
      "description": "Test tool for registry flow",
      "preview": "# rye:validated:2026-02-04T07:40:00Z:e8df58d7dd74cef449d96731b430a10a2b1696abc8558503ae4a2c910be96908|rye-registry@leolilley\n\"\"\"Test tool for registry flow validation.\n\nA simple Python tool to test pu",
      "source": "project",
      "path": "/home/leo/projects/ryeos/tests/e2e/.ai/tools/test/test_registry_tool.py",
      "score": 1.0,
      "category": "test",
      "signed": false,
      "type": "tool"
    }
  ],
  "total": 1282,
  "query": "*",
  "scope": "",
  "space": "all",
  "limit": 10,
  "offset": 0,
  "sear
... (truncated)
```

### Response — Turn 7

### Tool: rye_load

```json
{
  "item_id": "rye/core/create_directive",
  "item_type": "directive"
}
```

### Result

```
{
  "content": "# Create Directive\n\nCreate a new directive file with proper metadata, validate, and sign it.\n\n```xml\n<directive name=\"create_directive\" version=\"3.0.0\">\n  <metadata>\n    <description>Create a directive file with minimal required fields, check for duplicates, write to disk, and sign it.</description>\n    <category>rye/core</category>\n    <author>rye-os</author>\n    <model tier=\"fast\" />\n    <limits turns=\"6\" tokens=\"4096\" />\n    <permissions>\n      <search>\n        <directive>*</directive>\n      ... [truncated, 3168 chars total]"
}
```

### Response — Turn 8

### Tool: directive_return

```json
{
  "result": "This is a minimal directive where the LLM must infer tool usage from the capabilities block alone. The LLM called the following tools:\n- rye_file_system_ls\n- rye_file_system_glob \n- rye_file_system_grep\n- rye_file_system_read\n- rye_file_system_write\nIt also used rye_search and rye_load at least once.",
  "tools_used": "rye_file_system_ls, rye_file_system_glob, rye_file_system_grep, rye_file_system_read, rye_file_system_write, rye_search, rye_load"
}
```

### Result

```
{
  "result": "This is a minimal directive where the LLM must infer tool usage from the capabilities block alone. The LLM called the following tools:\n- rye_file_system_ls\n- rye_file_system_glob \n- rye_file_system_grep\n- rye_file_system_read\n- rye_file_system_write\nIt also used rye_search and rye_load at least once.",
  "tools_used": "rye_file_system_ls, rye_file_system_glob, rye_file_system_grep, rye_file_system_read, rye_file_system_write, rye_search, rye_load"
}
```

---

**Completed** -- 8 turns, 37207 tokens, $0.0103, 15.3s
