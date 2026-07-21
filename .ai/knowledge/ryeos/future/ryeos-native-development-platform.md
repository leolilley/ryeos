<!-- ryeos:signed:2026-07-21T00:24:56Z:ee87d9336abc4e439604daa5655aa2661e7892c15ff46750c82a1792ef87237a:oP5ogxrV87M5+l3/YUvIo90y8eayLI2z3nreGNlPQiONJ6puiYETu0CHtPNuaieNJH4RG6jCsdqJWHkUlGRFCw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: ryeos/future
name: ryeos-native-development-platform
title: RyeOS-Native Development Platform
description: Deferred direction for project hosting, checks, review, release, and GitHub projection
entry_type: design
version: "1.0.0"
```

# RyeOS-Native Development Platform

## Status

Deferred architecture direction. Not part of the current implementation.

## Direction

The current development loop still uses GitHub-era assumptions:

- source coordination through Git refs and PRs;
- broad validation through local test scripts or GitHub Actions;
- bundle publication that can mutate checked-in bundle trees;
- expensive test/build artifacts landing on the developer machine unless
  manually redirected;
- review discussion, checks, artifacts, and source changes split across tools.

Long term, a RyeOS node should be able to host a project, coordinate changes,
run checks, preserve review/provenance, publish artifacts, and mirror/export to
GitHub when needed.

GitHub concepts should become projections of richer RyeOS-native objects:

```diagram
╭──────────────────────────────╮
│ RyeOS project node            │
│ source snapshots, changes,    │
│ threads, checks, reviews,     │
│ artifacts, policies, releases │
╰──────────────┬───────────────╯
               │ mirror/export
               ▼
╭──────────────────────────────╮
│ GitHub compatibility          │
│ commits, PRs, actions,        │
│ releases, statuses            │
╰──────────────────────────────╯
```

## Near-term follow-ups

1. Isolate bundle population output so local validation does not dirty tracked
   bundle trees.
2. Wrap targeted test and explicit bundle-refresh workflows as RyeOS
   tools/services with artifact and target-dir handling.
3. Model check runs as durable thread/check records with logs in CAS.
4. Add scheduler-managed runner execution for expensive checks.
5. Add RyeOS UI/TUI review surfaces for change records and check results.
6. Only then consider a RyeOS-hosted remote that can mirror to GitHub.

## Non-goals

- Do not replace Git interoperability.
- Do not block current GitHub-based development.
- Do not move current source hosting until check/run/artifact isolation is
  reliable.
