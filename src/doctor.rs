//! Offline maintenance commands: `doctor`, `check --full`, `gc`, and quota
//! changes. Each acquires the same exclusive store lock as `serve`.

use std::collections::HashSet;
use std::io::Read;
use std::os::unix::fs::MetadataExt;

use rusqlite::Connection;
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::fsutil::{Area, DataDir, sync_dir};
use crate::ids::StorageId;
use crate::metadata::queries;
use crate::metadata::{self, migrations, now_ms, with_write_tx};
use crate::store::open_offline;

fn count(conn: &Connection, sql: &str) -> Result<i64> {
    Ok(conn.query_row(sql, [], |r| r.get(0))?)
}

/// Print a diagnostic report. Returns false when problems were found.
pub fn doctor(cfg: &Config, full: bool) -> Result<bool> {
    let (data, conn, meta, _) = open_offline(cfg, false)?;
    let mut problems: Vec<String> = Vec::new();
    println!("storlite {}", env!("CARGO_PKG_VERSION"));
    println!(
        "sqlite {} ({})",
        metadata::sqlite_version(),
        metadata::sqlite_source_id()
    );
    println!("data directory: {}", data.root().display());
    println!(
        "store id: {}  region: {}  format: {}",
        meta.store_id, meta.region, meta.format_version
    );
    let applied = migrations::verify(&conn)?;
    println!(
        "migrations applied: {:?} (latest known {})",
        applied,
        migrations::latest_version()
    );
    if applied.last().copied().unwrap_or(0) < migrations::latest_version() {
        println!("note: migrations pending; they are applied by `serve`");
    }
    let s = metadata::read_settings(&conn)?;
    println!(
        "settings: journal_mode={} synchronous={} foreign_keys={}",
        s.journal_mode, s.synchronous, s.foreign_keys
    );
    let ic: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    if ic != "ok" {
        problems.push(format!("SQLite integrity_check: {ic}"));
    }
    let fk = count(&conn, "SELECT count(*) FROM pragma_foreign_key_check")?;
    if fk > 0 {
        problems.push(format!("{fk} foreign key violations"));
    }
    problems.extend(queries::invariant_violations(&conn)?);
    println!(
        "buckets: {}  objects: {}  logical bytes: {}",
        count(&conn, "SELECT count(*) FROM buckets")?,
        count(&conn, "SELECT count(*) FROM objects")?,
        count(&conn, "SELECT coalesce(sum(logical_bytes), 0) FROM buckets")?
    );
    for st in ["writing", "ready", "garbage"] {
        println!(
            "blobs {st}: {}",
            count(
                &conn,
                &format!("SELECT count(*) FROM blobs WHERE state = '{st}'")
            )?
        );
    }
    for st in ["open", "completing", "completed", "aborted"] {
        println!(
            "multipart uploads {st}: {}",
            count(
                &conn,
                &format!("SELECT count(*) FROM multipart_uploads WHERE state = '{st}'")
            )?
        );
    }
    let writing = count(&conn, "SELECT count(*) FROM blobs WHERE state = 'writing'")?;
    let completing = count(
        &conn,
        "SELECT count(*) FROM multipart_uploads WHERE state = 'completing'",
    )?;
    if writing + completing > 0 {
        println!(
            "pending recovery: {writing} WRITING blobs, {completing} COMPLETING uploads (handled at next start or `gc --apply`)"
        );
    }
    if let Ok(st) = data.fs_stats() {
        println!(
            "filesystem: {} bytes available of {}, {} inodes available",
            st.avail_bytes, st.total_bytes, st.avail_inodes
        );
    }
    if full {
        verify_files(&data, &conn, &mut problems)?;
    }
    if problems.is_empty() {
        println!("result: OK");
        Ok(true)
    } else {
        println!("result: {} problem(s)", problems.len());
        for p in &problems {
            println!("  - {p}");
        }
        Ok(false)
    }
}

pub fn sha256_file(f: &mut std::fs::File) -> std::io::Result<(u64, [u8; 32])> {
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut n = 0u64;
    loop {
        let r = f.read(&mut buf)?;
        if r == 0 {
            break;
        }
        h.update(&buf[..r]);
        n += r as u64;
    }
    Ok((n, h.finalize().into()))
}

/// Hash every referenced file and classify untracked entries.
fn verify_files(data: &DataDir, conn: &Connection, problems: &mut Vec<String>) -> Result<()> {
    let referenced = queries::referenced_blobs(conn)?;
    let mut checked = 0u64;
    for (id, area, size, sha) in &referenced {
        match data.open_read(area.fs_area(), id) {
            Ok(mut f) => {
                let (n, digest) = sha256_file(&mut f)?;
                if n != *size {
                    problems.push(format!("{} {id}: size {n}, expected {size}", area.as_str()));
                } else if &digest != sha {
                    problems.push(format!("{} {id}: SHA-256 mismatch", area.as_str()));
                }
            }
            Err(e) => problems.push(format!(
                "{} {id}: cannot open referenced file: {e}",
                area.as_str()
            )),
        }
        checked += 1;
    }
    println!("verified {checked} referenced files");
    // Walk the fixed two-level layout without following symlinks.
    let mut tracked: HashSet<StorageId> = HashSet::new();
    {
        let mut stmt = conn.prepare("SELECT storage_id FROM blobs")?;
        let rows = stmt.query_map([], |r| r.get::<_, Vec<u8>>(0))?;
        for r in rows {
            if let Some(id) = StorageId::from_slice(&r?) {
                tracked.insert(id);
            }
        }
    }
    let mut untracked = 0u64;
    for area in Area::ALL {
        let root = data.root().join(area.dir_name());
        for l1 in std::fs::read_dir(&root)? {
            let l1 = l1?;
            if !l1.file_type()?.is_dir() {
                problems.push(format!("unexpected entry {}", l1.path().display()));
                continue;
            }
            for l2 in std::fs::read_dir(l1.path())? {
                let l2 = l2?;
                if !l2.file_type()?.is_dir() {
                    problems.push(format!("unexpected entry {}", l2.path().display()));
                    continue;
                }
                for f in std::fs::read_dir(l2.path())? {
                    let f = f?;
                    let name = f.file_name().to_string_lossy().into_owned();
                    let base = if area == Area::Staging {
                        name.strip_suffix(".tmp")
                    } else {
                        Some(name.as_str())
                    };
                    let id = base.and_then(StorageId::parse_hex);
                    let meta = std::fs::symlink_metadata(f.path())?;
                    let tracked_here = id.is_some_and(|i| tracked.contains(&i));
                    if !meta.is_file() || !tracked_here {
                        untracked += 1;
                        problems.push(format!(
                            "untracked {} {} ({} bytes, mode {:o}); not deleted",
                            if meta.is_file() { "file" } else { "entry" },
                            f.path().display(),
                            meta.len(),
                            meta.mode() & 0o7777
                        ));
                    }
                }
            }
        }
    }
    println!("untracked entries: {untracked}");
    Ok(())
}

/// Offline GC: run restart recovery, then reclaim eligible tracked garbage.
pub fn gc(cfg: &Config, apply: bool) -> Result<()> {
    let (data, mut conn, _, _) = open_offline(cfg, apply)?;
    // Offline, no process can hold a reader on garbage files, so the online
    // grace period does not apply.
    let now = i64::MAX;
    if apply {
        let r = with_write_tx(&mut conn, |tx| queries::recover(tx, now_ms()))?;
        println!(
            "recovery: reopened {} uploads, reclaimed {} WRITING blobs",
            r.reopened_uploads, r.reclaimed_writing_blobs
        );
    } else {
        println!(
            "would recover: {} WRITING blobs, {} COMPLETING uploads",
            count(&conn, "SELECT count(*) FROM blobs WHERE state = 'writing'")?,
            count(
                &conn,
                "SELECT count(*) FROM multipart_uploads WHERE state = 'completing'"
            )?
        );
    }
    let mut total = 0u64;
    let mut bytes = 0u64;
    loop {
        let batch = queries::garbage_batch(&conn, now, 1000)?;
        if batch.is_empty() {
            break;
        }
        if !apply {
            for (id, area, size) in batch.iter().take(20) {
                println!("eligible: {} {id} ({size} bytes)", area.as_str());
            }
            total += batch.len() as u64;
            bytes += batch.iter().map(|b| b.2).sum::<u64>();
            if batch.len() < 1000 {
                break;
            }
            // Dry run cannot advance past the first page without deleting.
            println!("(dry run lists only the first 1000 eligible blobs)");
            break;
        }
        let mut dirs = Vec::new();
        for (id, area, size) in &batch {
            for a in [area.fs_area(), Area::Staging] {
                if let Some(d) = data.remove(a, id)? {
                    dirs.push(d);
                }
            }
            total += 1;
            bytes += size;
        }
        for d in &dirs {
            sync_dir(d)?;
        }
        with_write_tx(&mut conn, |tx| {
            for (id, _, _) in &batch {
                queries::delete_garbage_row(tx, id)?;
            }
            Ok(())
        })?;
    }
    let pending: i64 = count(&conn, "SELECT count(*) FROM blobs WHERE state = 'garbage'")?;
    if apply {
        println!("reclaimed {total} garbage blobs ({bytes} bytes); {pending} not yet eligible");
    } else {
        println!(
            "dry run: {total} eligible garbage blobs ({bytes} bytes); {pending} garbage rows total"
        );
    }
    Ok(())
}

pub fn set_quota(cfg: &Config, name: &str, quota: Option<u64>) -> Result<()> {
    let (_data, mut conn, _, _) = open_offline(cfg, true)?;
    let ok = with_write_tx(&mut conn, |tx| queries::set_bucket_quota(tx, name, quota))?;
    if !ok {
        return Err(Error::other(format!("no such bucket: {name}")));
    }
    Ok(())
}
