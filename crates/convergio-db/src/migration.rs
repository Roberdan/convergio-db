//! Migration runner — collects migrations from extensions, applies in order.
//!
//! Each extension declares its migrations via Extension::migrations().
//! The runner tracks applied versions in `_schema_registry`.

use convergio_types::extension::Migration;
use rusqlite::Connection;

/// Ensures the schema registry table exists.
pub fn ensure_registry(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS _schema_registry (
            module TEXT PRIMARY KEY,
            version INTEGER NOT NULL,
            applied_at TEXT NOT NULL DEFAULT (datetime('now'))
        )",
    )
}

/// Returns the current version for a module, or 0 if not yet applied.
pub fn current_version(conn: &Connection, module: &str) -> rusqlite::Result<u32> {
    let mut stmt = conn.prepare("SELECT version FROM _schema_registry WHERE module = ?1")?;
    match stmt.query_row([module], |row| row.get::<_, u32>(0)) {
        Ok(v) => Ok(v),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(0),
        Err(e) => Err(e),
    }
}

/// Apply pending migrations for a module.
/// Migrations must be sorted by version (ascending).
/// Returns the number of migrations applied.
///
/// Each statement is executed individually so that ALTER TABLE ADD COLUMN
/// errors caused by duplicate columns are silently skipped. This makes
/// migrations idempotent even when a previous run crashed mid-way and left
/// the schema in a half-applied state.
pub fn apply_migrations(
    conn: &Connection,
    module: &str,
    migrations: &[Migration],
) -> rusqlite::Result<usize> {
    ensure_registry(conn)?;
    let current = current_version(conn, module)?;
    let mut applied = 0;

    for m in migrations {
        if m.version <= current {
            continue;
        }
        tracing::info!(
            module,
            version = m.version,
            desc = m.description,
            "applying migration"
        );
        apply_sql_idempotent(conn, m.up)?;
        conn.execute(
            "INSERT OR REPLACE INTO _schema_registry (module, version) VALUES (?1, ?2)",
            rusqlite::params![module, m.version],
        )?;
        applied += 1;
    }
    Ok(applied)
}

/// Execute migration SQL, tolerating "duplicate column name" errors.
///
/// Strategy: try the whole batch first (fast path). If it fails with a
/// duplicate-column error, fall back to statement-by-statement execution
/// so only the offending ALTER TABLE is skipped while the rest succeeds.
///
/// The statement splitter is trigger-aware: semicolons inside
/// BEGIN...END blocks are not treated as statement boundaries.
fn apply_sql_idempotent(conn: &Connection, sql: &str) -> rusqlite::Result<()> {
    // Fast path — works for all migrations without duplicate columns.
    match conn.execute_batch(sql) {
        Ok(()) => return Ok(()),
        Err(e) if is_duplicate_column_error(&e) => {
            // Fall through to statement-by-statement retry.
        }
        Err(e) => return Err(e),
    }

    for stmt in split_statements(sql) {
        let trimmed = stmt.trim();
        if trimmed.is_empty() {
            continue;
        }
        match conn.execute_batch(trimmed) {
            Ok(()) => {}
            Err(e) if is_duplicate_column_error(&e) => {
                tracing::debug!("skipping already-applied: {trimmed}");
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Split SQL into top-level statements on `;`, but keep semicolons
/// inside `BEGIN ... END` blocks intact (for triggers / compound stmts).
fn split_statements(sql: &str) -> Vec<&str> {
    let mut stmts = Vec::new();
    let mut start = 0;
    let mut depth = 0u32;
    let upper = sql.to_ascii_uppercase();
    let bytes = upper.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        if i + 5 <= len && &bytes[i..i + 5] == b"BEGIN" && is_word_boundary(bytes, i, 5, len) {
            depth += 1;
            i += 5;
        } else if i + 3 <= len && &bytes[i..i + 3] == b"END" && is_word_boundary(bytes, i, 3, len) {
            depth = depth.saturating_sub(1);
            i += 3;
        } else if bytes[i] == b';' && depth == 0 {
            stmts.push(&sql[start..i]);
            start = i + 1;
            i += 1;
        } else {
            i += 1;
        }
    }
    if start < len {
        stmts.push(&sql[start..]);
    }
    stmts
}

/// Returns true if position `pos..pos+word_len` sits on word boundaries.
fn is_word_boundary(bytes: &[u8], pos: usize, word_len: usize, len: usize) -> bool {
    let before_ok = pos == 0 || !bytes[pos - 1].is_ascii_alphanumeric();
    let after = pos + word_len;
    let after_ok = after >= len || !bytes[after].is_ascii_alphanumeric();
    before_ok && after_ok
}

/// Returns true if the error is a "duplicate column name" from ALTER TABLE.
fn is_duplicate_column_error(e: &rusqlite::Error) -> bool {
    e.to_string().contains("duplicate column name")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
        conn
    }

    #[test]
    fn registry_created_on_first_run() {
        let conn = test_conn();
        ensure_registry(&conn).unwrap();
        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM sqlite_master WHERE name = '_schema_registry'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(exists);
    }

    #[test]
    fn applies_migrations_in_order() {
        let conn = test_conn();
        let migrations = vec![
            Migration {
                version: 1,
                description: "create users",
                up: "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)",
            },
            Migration {
                version: 2,
                description: "add email",
                up: "ALTER TABLE users ADD COLUMN email TEXT",
            },
        ];
        let applied = apply_migrations(&conn, "test-mod", &migrations).unwrap();
        assert_eq!(applied, 2);
        assert_eq!(current_version(&conn, "test-mod").unwrap(), 2);
    }

    #[test]
    fn skips_already_applied() {
        let conn = test_conn();
        let migrations = vec![Migration {
            version: 1,
            description: "create table",
            up: "CREATE TABLE t1 (id INTEGER PRIMARY KEY)",
        }];
        apply_migrations(&conn, "mod-a", &migrations).unwrap();
        let applied = apply_migrations(&conn, "mod-a", &migrations).unwrap();
        assert_eq!(applied, 0);
    }

    #[test]
    fn duplicate_column_is_idempotent() {
        let conn = test_conn();
        // Simulate a partially-applied migration: column exists but version
        // was never recorded (crash between execute_batch and registry write).
        conn.execute_batch("CREATE TABLE t2 (id INTEGER PRIMARY KEY)")
            .unwrap();
        conn.execute_batch("ALTER TABLE t2 ADD COLUMN extra TEXT")
            .unwrap();
        // Now run a migration that tries to add the same column.
        let migrations = vec![Migration {
            version: 1,
            description: "add extra col",
            up: "ALTER TABLE t2 ADD COLUMN extra TEXT",
        }];
        let applied = apply_migrations(&conn, "mod-dup", &migrations).unwrap();
        assert_eq!(applied, 1);
        assert_eq!(current_version(&conn, "mod-dup").unwrap(), 1);
    }
}
