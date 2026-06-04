# RyeOS-native development platform

## Status

Deferred architecture direction. Not part of the current implementation.

## Direction

The current development loop still uses GitHub-era assumptions:

- source coordination through Git refs and PRs;
- broad validation through local `scripts/gate.sh` or GitHub Actions;
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
2. Wrap `scripts/gate.sh` as a RyeOS tool/service with explicit artifact and
   target-dir handling.
3. Model check runs as durable thread/check records with logs in CAS.
4. Add scheduler-managed runner execution for expensive checks.
5. Add Studio/TUI review surfaces for change records and check results.
6. Only then consider a RyeOS-hosted remote that can mirror to GitHub.

## Non-goals

- Do not replace Git interoperability.
- Do not block current GitHub-based development.
- Do not move current source hosting until check/run/artifact isolation is
  reliable.
