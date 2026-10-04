//! Versioned, checksummed schema migrations.

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

pub struct Migration {
    pub version: i64,
    pub name: &'static str,
    pub sql: &'static str,
}

pub const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    name: "initial",
    sql: include_str!("../../migrations/0001_initial.sql"),
}];

/// On-disk storage format understood by this executable.
pub const FORMAT_VERSION: i64 = 1;

pub fn latest_version() -> i64 {
    MIGRATIONS.last().map(|m| m.version).unwrap_or(0)
}

fn checksum(sql: &str) -> [u8; 32] {
    Sha256::digest(sql.as_bytes()).into()
}

/// Applied migration versions with their recorded checksums.
pub fn applied(conn: &Connection) -> Result<Vec<(i64, Vec<u8>)>> {
    let exists: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_migrations'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if exists.is_none() {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare("SELECT version, checksum_sha256 FROM schema_migrations ORDER BY version")?;
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Verify recorded migrations and refuse stores written by a newer executable.
pub fn verify(conn: &Connection) -> Result<Vec<i64>> {
    let applied = applied(conn)?;
    let mut versions = Vec::new();
    for (version, sum) in &applied {
        let Some(m) = MIGRATIONS.iter().find(|m| m.version == *version) else {
            return Err(Error::config(format!(
                "metadata schema version {version} is newer than this executable supports ({})",
                latest_version()
            )));
        };
        if sum.as_slice() != checksum(m.sql) {
            return Err(Error::integrity(format!(
                "migration {version} ({}) checksum does not match this executable",
                m.name
            )));
        }
        versions.push(*version);
    }
    Ok(versions)
}

/// Apply all missing migrations, each in its own immediate transaction.
pub fn apply(conn: &Connection) -> Result<usize> {
    let done = verify(conn)?;
    let mut count = 0;
    for m in MIGRATIONS.iter().filter(|m| !done.contains(&m.version)) {
        let tx = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
        tx.execute_batch(m.sql)?;
        tx.execute(
            "INSERT INTO schema_migrations(version, name, checksum_sha256, applied_at_ms) VALUES (?1, ?2, ?3, ?4)",
            params![m.version, m.name, checksum(m.sql).to_vec(), super::now_ms()],
        )?;
        tx.commit()?;
        count += 1;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = super::super::create_database(&dir.path().join("m.sqlite3")).unwrap();
        (dir, conn)
    }

    #[test]
    fn migrations_are_repeatable() {
        let (_d, conn) = fresh();
        assert_eq!(apply(&conn).unwrap(), MIGRATIONS.len());
        assert_eq!(apply(&conn).unwrap(), 0);
        let ok: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0)).unwrap();
        assert_eq!(ok, "ok");
        let fk: i64 = conn
            .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fk, 0);
    }

    #[test]
    fn newer_schema_is_rejected() {
        let (_d, conn) = fresh();
        apply(&conn).unwrap();
        conn.execute(
            "INSERT INTO schema_migrations VALUES (99, 'future', zeroblob(32), 0)",
            [],
        )
        .unwrap();
        assert!(apply(&conn).is_err());
    }

    #[test]
    fn tampered_migration_checksum_is_rejected() {
        let (_d, conn) = fresh();
        apply(&conn).unwrap();
        conn.execute("UPDATE schema_migrations SET checksum_sha256 = zeroblob(32)", [])
            .unwrap();
        assert!(matches!(verify(&conn), Err(Error::Integrity(_))));
    }

    #[test]
    fn schema_constraints_reject_invalid_rows() {
        let (_d, conn) = fresh();
        apply(&conn).unwrap();
        let bad = [
            "INSERT INTO buckets(id, name, created_at_ms) VALUES (x'00', 'abc', 0)",
            "INSERT INTO buckets(id, name, created_at_ms) VALUES (zeroblob(16), 'ab', 0)",
            "INSERT INTO blobs(storage_id, area, state, created_at_ms) VALUES (zeroblob(16), 'other', 'writing', 0)",
            "INSERT INTO blobs(storage_id, area, state, created_at_ms) VALUES (zeroblob(16), 'object', 'ready', 0)",
            "INSERT INTO blobs(storage_id, area, state, created_at_ms) VALUES (zeroblob(16), 'object', 'garbage', 0)",
            "INSERT INTO objects(bucket_id, object_key, storage_id, generation_id, etag, last_modified_ms) VALUES (zeroblob(16), x'61', zeroblob(16), zeroblob(16), 'e', 0)",
        ];
        for sql in bad {
            assert!(conn.execute(sql, []).is_err(), "{sql}");
        }
    }
}
