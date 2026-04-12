# ADR-034: Idempotent Database Migrations

**Status**: Accepted
**Date**: 2026-04-07

## Context

Two bugs were discovered in the daemon's local SQLite migration system:

1. **Migration v22 duplicate column**: On every daemon boot, migration v22
   (orchestrator module) tries to `ALTER TABLE waves ADD COLUMN updated_at`
   and `ALTER TABLE tasks ADD COLUMN updated_at`. If the daemon previously
   crashed after executing these statements but before recording the version
   in `_schema_registry`, the next boot retries v22 and fails with
   `duplicate column name: updated_at`.

2. **ipc_file_locks missing expires_at**: The orchestrator reaper runs
   `DELETE FROM ipc_file_locks WHERE expires_at IS NOT NULL AND expires_at < datetime('now')`
   every 5 minutes, but the `ipc_file_locks` table was never given an
   `expires_at` column. This produces `WARN reaper: expired lock cleanup:
   no such column: expires_at` in the logs.

## Decision

### Fix pattern: statement-level idempotent execution

The migration runner (`convergio_db::migration::apply_migrations`) now
executes each SQL statement individually (split on `;`) instead of passing
the entire migration as a single `execute_batch`. If a statement fails with
a "duplicate column name" error, it is silently skipped.

This makes all `ALTER TABLE ADD COLUMN` migrations inherently idempotent,
regardless of whether a previous run partially applied the schema.

### ipc_file_locks.expires_at

A new IPC migration (version 2) adds the missing `expires_at TEXT` column
to `ipc_file_locks`, matching the reaper's query expectations.

## Consequences

- All existing and future ALTER TABLE ADD COLUMN migrations are safe to
  re-run on partially-applied schemas.
- The reaper's expired-lock cleanup now works correctly.
- No changes to the Migration struct or extension API are needed.
- Other types of partial failures (e.g., CREATE TABLE without IF NOT EXISTS)
  still require defensive SQL; this fix only handles the column-addition case.
