use rusqlite::{Connection, params};

use crate::error::StoreError;
use crate::time::now_ms;

/// The version 1 schema, applied to a fresh database.
const SCHEMA_V1: &str = include_str!("schema.sql");

/// The upper bound on the migration list length, a fail-fast guard on the
/// apply loop set far above any plausible number of migrations.
const MIGRATION_COUNT_MAX: u32 = 256;

/// An ordered, idempotent schema change. Versions are contiguous from 1.
struct Migration {
    version: u32,
    description: &'static str,
    sql: &'static str,
}

const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    description: "initial schema",
    sql: SCHEMA_V1,
}];

/// The applied schema version after running every migration newer than the
/// database's current version, in order.
pub(crate) fn apply(connection: &Connection) -> Result<u32, StoreError> {
    assert!(!MIGRATIONS.is_empty(), "at least one migration must be defined");

    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_versions (
            version    INTEGER PRIMARY KEY,
            applied_at INTEGER NOT NULL,
            description TEXT
        );",
    )?;

    let current = current_version(connection)?;
    let mut last = current;
    let mut iterations: u32 = 0;

    for migration in MIGRATIONS {
        iterations += 1;

        assert!(
            iterations <= MIGRATION_COUNT_MAX,
            "migration loop exceeded {MIGRATION_COUNT_MAX} iterations",
        );

        if migration.version <= current {
            continue;
        }

        assert!(
            migration.version == last + 1,
            "migrations must be contiguous from the current version",
        );

        connection.execute_batch(migration.sql)?;

        connection.execute(
            "INSERT INTO schema_versions (version, applied_at, description) VALUES (?1, ?2, ?3)",
            params![migration.version, now_ms()?, migration.description],
        )?;

        last = migration.version;
    }

    let resulting = current_version(connection)?;

    assert!(resulting >= current, "schema version must not regress");

    Ok(resulting)
}

/// The highest applied schema version, or 0 when none has been applied.
/// The `schema_versions` table must already exist.
pub(crate) fn current_version(connection: &Connection) -> Result<u32, StoreError> {
    let highest: Option<i64> = connection.query_row(
        "SELECT MAX(version) FROM schema_versions",
        [],
        |row| row.get(0),
    )?;

    let highest = highest.unwrap_or(0);

    assert!(highest >= 0, "schema version must be non-negative");

    let version = u32::try_from(highest).map_err(|_| StoreError::CorruptSchemaVersion(highest))?;

    Ok(version)
}
