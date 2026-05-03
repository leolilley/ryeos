# Recovering from foreign-schema bail

When `ryeosd` starts, it verifies that every SQLite database it owns
(`runtime.db`, `projection.db`) was created by this daemon. If the
schema doesn't match — wrong `application_id`, missing columns,
unexpected tables or indexes — the daemon refuses to start with a
typed error that includes the offending file path.

## Symptoms

```
error: database application_id is 0, expected 1353363585;
       this file (/path/to/runtime.db) was not created by this daemon.
       Recovery: mv <file> <file>.foreign.$(date +%s);
       then restart with --init-if-missing.
```

Or:

```
error: unexpected user table 'legacy_data' in database
       (/path/to/projection.db)
```

## Recovery procedure

1. **Stop the daemon** (it already failed to start).

2. **Identify the offending file** from the error message.
   The path is always included in the bail output.

3. **Archive the file** by renaming it:
   ```sh
   mv /path/to/offending.db /path/to/offending.db.foreign.$(date +%s)
   ```

4. **Restart** with `--init-if-missing`:
   ```sh
   ryeosd --init-if-missing
   ```

   The daemon will create a fresh database with the correct schema.

5. **Verify** the daemon starts cleanly. Check logs for
   `schema verification passed` or equivalent startup messages.

## What you lose

Archiving the foreign database removes all persisted thread state,
event history, and projection data for that database. The daemon
rebuilds from scratch.

## When this happens

Common causes:

- The state directory was pointed at a file from a different daemon
  instance or a different version of `ryeosd`.
- A manual SQLite operation altered the schema outside the daemon.
- The `application_id` PRAGMA was cleared by an external tool.

## Prevention

- Never share state directories between daemon instances.
- Never modify daemon-owned SQLite files with external tools.
- Use `--bind` and `--state-dir` consistently across restarts.
