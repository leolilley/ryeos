<!-- ryeos:signed:2026-06-09T08:26:09Z:5f7f34ead159b2cc0e37ea8cbb85d5a41710d16500977ab68e1e3de6af7841ed:oqig+AHifZEAQDotr2FM/8fkTeOVKJAkfuOXhdO8QX4bRo4ObZduwPEhxYnFIINcC4eN6n+UrjS2h66GLtzHDw==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->
# Future: Remote config advanced hardening

## Status

Deferred advanced work. The current remote-config cleanup deliberately keeps the
near-term model simple:

```text
remote definitions: user config + optional project definition override
project bindings: user/operator-local only
writes: remote configure / bind-project write user config only
```

This is sufficient for the current cleanup: there is no `.ai/config/remotes/**`
local-only signing hole, no project `.ai` mutation from operator workflows, and
no compatibility path for singular `project_binding` or partial invalid remote
configs.

## Current baseline

- User/operator remotes live at:

  ```text
  <system_space_dir>/.ai/config/remotes/remotes.yaml
  ```

- Project remotes may exist at:

  ```text
  <project>/.ai/config/remotes/remotes.yaml
  ```

  They are treated as read-only remote definitions. They can override same-name
  connection metadata for diagnostics and read paths.

- User/operator `project_bindings` are applied on top of the effective remote
  definition after project override.

- Project-authored `project_bindings` are invalid. Local project-to-remote path
  bindings are operator state, not portable project content.

- `remote configure` always writes user/operator config.

- `remote bind-project` always writes user/operator config. If a remote exists
  only in project config, bind-project copies the connection definition into
  user config, clears any bindings, and then writes the requested operator-local
  binding.

## Advanced path 1: runtime trust enforcement for project remotes

### Trigger

Implement this only if project remotes become a security-sensitive distribution
surface at runtime, not merely signed project/bundle content that is verified by
packaging/preflight workflows.

Examples that should pull this forward:

- hosted nodes consume project remote definitions from untrusted workspaces;
- remote definitions can affect automatic forwarding, deployment, or secrets;
- operators need runtime diagnostics that prove the project remotes file was
  signed by an accepted key before it influenced command behavior.

### Shape

Do not add another ad-hoc YAML verifier. Route project remote loading through the
same signed item machinery used by normal project/bundle content:

1. Define or reuse a first-class signed config kind for `.ai/config/**`.
2. Resolve project remotes by canonical item path/ref, not direct `read_to_string`
   from an arbitrary path.
3. Verify:
   - signature envelope;
   - signer trust class;
   - path anchoring under `.ai/config/remotes/remotes.yaml`;
   - declared `category: remotes` if present;
   - strict `RemoteConfig` validation.
4. Return invalid-project-config diagnostics that tell the operator to edit or
   remove the project config and re-sign it through normal signing tools.

### Guardrails

- Do not reintroduce local-only skips for `.ai/config/remotes/**`.
- Do not let runtime verification mutate or repair project `.ai`.
- Do not accept unsigned project remotes in one codepath and signed project
  remotes in another.
- Do not merge project-authored bindings. Project remotes may define connection
  metadata only.

## Advanced path 2: legacy binding salvage from invalid user config

### Trigger

Do not implement this unless there is a real upgrade requirement to preserve
operator-local bindings from malformed historical user remote entries.

The current strict cleanup intentionally does not salvage data from invalid user
remote records. If an old user remote is malformed, it is skipped and reported as
invalid. This matches the no-legacy-compatibility constraint for the cleanup.

### Shape if required later

If product support requires salvage:

1. Keep normal strict loading as the primary path.
2. For invalid **user-scope only** entries, parse just `project_bindings` from
   the raw YAML value.
3. Validate every recovered binding with `validate_remote_project_path` and
   canonical local path rules.
4. Never recover singular `project_binding`.
5. Never recover bindings from project-scope remotes.
6. Emit a diagnostic that the connection definition is invalid and must be
   repaired with `ryeos remote configure`.

### Guardrails

- This must be an explicit migration/salvage mode, not normal compatibility.
- Salvaged bindings must not make the invalid remote itself usable.
- Salvage must not write project config.

## Advanced path 3: atomic and concurrent remote config writes

### Trigger

Implement this when multiple daemon requests can concurrently mutate remotes in
practice, or when remote config writes become important enough to require
crash-safe durability guarantees.

Examples:

- UI can run `remote configure` and `remote bind-project` concurrently;
- background remote repair jobs update config;
- hosted/operator workflows edit many remote bindings programmatically.

### Shape

Add one shared write primitive for remotes config:

```text
load under lock → validate mutation → write temp file → fsync → atomic rename
```

Recommended pieces:

1. File lock scoped to `<system_space_dir>/.ai/config/remotes/remotes.yaml`.
2. Re-read under lock before mutation to avoid lost updates.
3. Validate all remotes before writing.
4. Write to a sibling temp file.
5. Flush/fsync file and containing directory where supported.
6. Atomic rename into place.
7. Keep the function user/operator scoped; never use it for project `.ai` writes.

### Guardrails

- Do not expose a generic `save_remotes_for_scope` again.
- Do not add project-scope write support as part of atomicity.
- Keep project read layering separate from user/operator mutation.

## Relationship to routes

Routes and remotes intentionally use different composition models.

Routes are additive node-config records loaded from system space and effective
bundle roots. The route table rejects duplicate ids and method/path collisions.

Remotes are keyed connection definitions. Same-name project remotes may override
connection metadata, while operator-local bindings are overlaid after definition
resolution. Making remotes fully additive would make a command like `remote prod`
ambiguous; silently merging project-authored bindings would violate the local
operator boundary.

## Do not revive

- `.ai/config/remotes/**` signing/export/install/preflight skips.
- Project `.ai` mutation from `remote configure` or `remote bind-project`.
- `.ryeos/` side-channel remote config.
- Singular `project_binding`.
- Partial invalid remote configs with empty principal/signing keys.
- `MISSING_SITE_ID_SENTINEL`-style compatibility paths.
