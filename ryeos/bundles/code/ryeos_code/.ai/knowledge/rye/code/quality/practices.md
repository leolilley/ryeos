<!-- rye:signed:2026-03-03T22:52:58Z:94da6142c8cbf7929e2a0bab6b8b0cb36774961c1a4d8e7240576b711149cf71:-cX_lXfe2eTr1JVk6gLwOfa2AoIcxoqSbrouEXupbYqwhmZSodS8ZSyctJoBCY7eL7oQxbWcppA0QhgKG3hcCQ==:4b987fd4e40303ac -->
<!-- rye:unsigned -->

```yaml
name: practices
title: Anti-Slop Coding Practices
entry_type: reference
category: rye/code/quality
version: "1.0.0"
author: rye-os
created_at: 2026-03-04T00:00:00Z
tags:
  - quality
  - anti-slop
  - practices
  - code-standards
```

# Anti-Slop Coding Practices

Rules for producing clean, minimal, convention-following code.

## 1. Follow Existing Patterns

Before writing any code, read the surrounding files. Match:
- Naming conventions (casing, prefixes, suffixes)
- Import style and ordering
- Error handling patterns
- File and directory structure
- Framework and library choices already in use

Do not introduce a new library when an existing one covers the use case. Do not introduce a new pattern when the codebase already has an established way of doing the same thing.

## 2. Minimal Diffs

Make the smallest change that solves the problem. Every added line must be justified by the task requirements.

- Do not refactor surrounding code while fixing a bug.
- Do not add "while I'm here" improvements.
- Do not restructure files, rename variables, or reformat code outside the change scope.
- If a function needs one new parameter, add one parameter — do not redesign the function signature.

## 3. No Over-Engineering

Build exactly what was asked for. Do not design for hypothetical future requirements.

- No abstraction layers for a single implementation.
- No configuration for values that are only used once.
- No generic frameworks where a simple function would suffice.
- No design patterns (Factory, Strategy, Builder, Visitor) unless the problem genuinely has multiple variants that exist today.
- No deeply nested generic types. If the type signature is hard to read, the design is too complex.

## 4. No Unnecessary Abstractions

Every new file, class, or module must have a clear reason to exist.

- If a function is called from one place, it should probably live in that file, not a new utility module.
- If a class wraps a single function call, delete the class and call the function directly.
- Helper files, utility modules, and shared libraries are only justified when they have multiple, independent callers.

## 5. Test With Real Implementations

Never mock what you can use for real.

- Use real database connections (SQLite in-memory, test containers) over database mocks.
- Use real file systems (temp directories) over filesystem mocks.
- Use real HTTP calls to test servers over request mocks.
- Only mock external services you cannot control (third-party APIs with rate limits, paid services).
- If a test requires more than 3 mocks, the code under test probably has a design problem. Fix the design, not the test.

## 6. All Tests Pass Before Handoff

Never hand off work with failing tests.

- Run the full test suite before declaring a task complete.
- If your change breaks existing tests, fix them as part of the change.
- If tests are flaky, investigate and fix the flakiness — do not skip or ignore them.
- If a test is genuinely obsolete due to your change, delete it and document why.

## 7. No Dead Code

Do not add code that is not immediately used.

- No commented-out code blocks. If it's not needed, delete it.
- No unused imports, variables, or functions.
- No "placeholder" implementations that will be filled in later.
- No backwards-compatibility shims for code that was just written.

## 8. Style Consistency

Match the surrounding code's style exactly.

- If the file uses single quotes, use single quotes.
- If the file uses 2-space indentation, use 2-space indentation.
- If the file puts braces on the same line, put braces on the same line.
- Run the project's linter and formatter before declaring the task complete.
