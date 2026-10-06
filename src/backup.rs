//! Consistent, verifiable backup (offline, or online from the running
//! server) and restore.
//!
//! A backup is a new mode-0700 directory containing a SQLite snapshot, every
//! file referenced by that snapshot (objects and committed parts) in the same
//! sharded layout, a JSON-lines file manifest, and a versioned manifest. It is
//! marked `BACKUP_INCOMPLETE` until every file and directory entry is durable;
//! only then is `BACKUP_COMPLETE` published. Access keys live in the
//! database and are therefore included (encrypted unless the store uses
//! plaintext protection); the master key and the configuration are not.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::fsutil::{self, Area, DataDir, open_dir, sync_dir, sync_fd};
use crate::ids::StorageId;
use crate::metadata::queries::{self, BlobArea, StoreMeta};
use crate::metadata::{self, migrations, now_ms, with_write_tx};
use crate::secrets::{MasterKey, SecretCodec};
use crate::store::{BackupPin, Store, open_offline};

/// Backup format marker.
pub const FORMAT: &str = "litebucket-backup-v1";
const INCOMPLETE: &str = "BACKUP_INCOMPLETE";
const COMPLETE: &str = "BACKUP_COMPLETE";
const MANIFEST: &str = "manifest.json";
const FILES: &str = "files.jsonl";

#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub format: String,
    pub store_id: String,
    pub region: String,
    pub storage_format: i64,
    pub created_at_ms: i64,
    pub litebucket_version: String,
    pub sqlite_version: String,
    pub database_sha256: String,
    pub database_bytes: u64,
    pub files_sha256: String,
    pub file_count: u64,
    pub file_bytes: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct FileEntry {
    area: String,
    id: String,
    size: u64,
    sha256: String,
}

fn area_of(name: &str) -> Result<BlobArea> {
    match name {
        "object" => Ok(BlobArea::Object),
        "part" => Ok(BlobArea::Part),
        _ => Err(Error::integrity(format!(
            "unknown area {name} in backup manifest"
        ))),
    }
}

fn create_new_file(path: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(path)?)
}

fn open_nofollow(path: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(path)?)
}

/// Copy exactly `size` bytes, returning the SHA-256 of what was copied.
fn copy_hashed(src: &mut File, dst: &mut File, size: u64) -> Result<[u8; 32]> {
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut remaining = size;
    while remaining > 0 {
        let want = remaining.min(buf.len() as u64) as usize;
        let n = src.read(&mut buf[..want])?;
        if n == 0 {
            return Err(Error::integrity("file shorter than its recorded size"));
        }
        h.update(&buf[..n]);
        dst.write_all(&buf[..n])?;
        remaining -= n as u64;
    }
    if src.read(&mut [0u8; 1])? != 0 {
        return Err(Error::integrity("file longer than its recorded size"));
    }
    Ok(h.finalize().into())
}

fn hash_file(path: &Path) -> Result<(u64, String)> {
    let mut f = open_nofollow(path)?;
    let (n, d) = crate::doctor::sha256_file(&mut f)?;
    Ok((n, hex::encode(d)))
}

/// Ensure `root/area/aa/bb` exists (0700) and return it, syncing new entries.
fn shard_dir(
    root: &Path,
    area: Area,
    id: &StorageId,
    created: &mut Vec<PathBuf>,
) -> Result<PathBuf> {
    let rel = fsutil::relative_path(area, id);
    let dir = root.join(rel.parent().expect("sharded path has parents"));
    if !dir.exists() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)?;
        created.push(dir.clone());
    }
    Ok(dir)
}

fn sync_tree_dirs(paths: &[PathBuf], root: &Path) -> Result<()> {
    // Sync each created leaf and its ancestors up to the root.
    let mut seen = std::collections::HashSet::new();
    for p in paths {
        let mut cur = Some(p.as_path());
        while let Some(d) = cur {
            if !d.starts_with(root) || !seen.insert(d.to_path_buf()) {
                break;
            }
            sync_dir(open_dir(d)?)?;
            cur = d.parent();
        }
    }
    sync_dir(open_dir(root)?)?;
    Ok(())
}

/// What a finished backup holds.
#[derive(Debug, Serialize, Deserialize)]
pub struct Summary {
    pub destination: PathBuf,
    pub store_id: String,
    pub file_count: u64,
    pub file_bytes: u64,
    /// Access-key secrets stored in plaintext in the backup.
    pub plaintext_secrets: i64,
    /// Access-key secrets encrypted with the master key, which is not included.
    pub encrypted_secrets: i64,
}

impl Summary {
    pub fn print(&self) {
        println!(
            "backup complete: {} files, {} bytes, store {} -> {}",
            self.file_count,
            self.file_bytes,
            self.store_id,
            self.destination.display()
        );
        if self.plaintext_secrets > 0 {
            println!(
                "WARNING: this backup contains {} access-key secret(s) in plaintext; protect it like a password store",
                self.plaintext_secrets
            );
        }
        if self.encrypted_secrets > 0 {
            println!(
                "note: access keys are included, encrypted; restoring them needs the master key, which is NOT in the backup"
            );
        }
        println!(
            "note: the configuration and the master key file are not included; back them up separately"
        );
    }
}

/// The destination as an absolute path: new, and outside the data directory.
fn check_destination(data_dir: &Path, destination: &Path) -> Result<PathBuf> {
    let dest_abs = if destination.is_absolute() {
        destination.to_path_buf()
    } else {
        std::env::current_dir()?.join(destination)
    };
    if dest_abs.starts_with(data_dir) {
        return Err(Error::config(
            "the backup destination must not be inside the data directory",
        ));
    }
    if dest_abs.exists() {
        return Err(Error::config(format!(
            "{} already exists; backups go to a new directory",
            dest_abs.display()
        )));
    }
    Ok(dest_abs)
}

/// Create the destination, marked incomplete until [`finish`] publishes it.
fn begin(dest: &Path) -> Result<()> {
    fs::DirBuilder::new().mode(0o700).create(dest)?;
    let mut marker = create_new_file(&dest.join(INCOMPLETE))?;
    writeln!(marker, "backup in progress; do not restore")?;
    sync_fd(&marker)?;
    sync_dir(open_dir(dest)?)?;
    Ok(())
}

/// Snapshot the metadata into the destination; one consistent read.
fn snapshot(conn: &rusqlite::Connection, dest: &Path) -> Result<()> {
    let db_dest = dest.join(fsutil::DB_FILE);
    conn.execute("VACUUM INTO ?1", [db_dest.to_string_lossy().as_ref()])?;
    sync_fd(open_nofollow(&db_dest)?)?;
    Ok(())
}

/// Copy and verify every file the snapshot references, then write the
/// manifests and publish `BACKUP_COMPLETE`.
fn finish(data: &DataDir, meta: &StoreMeta, dest_abs: &Path) -> Result<Summary> {
    let db_dest = dest_abs.join(fsutil::DB_FILE);
    let snapshot = rusqlite::Connection::open_with_flags(
        &db_dest,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let referenced = queries::referenced_blobs(&snapshot)?;
    drop(snapshot);

    let mut files = create_new_file(&dest_abs.join(FILES))?;
    let mut files_hash = Sha256::new();
    let mut created = Vec::new();
    let (mut count, mut bytes) = (0u64, 0u64);
    for (id, area, size, sha) in &referenced {
        let fs_area = area.fs_area();
        let mut src = data.open_read(fs_area, id).map_err(|e| {
            Error::integrity(format!(
                "referenced {} {id} cannot be opened: {e}",
                area.as_str()
            ))
        })?;
        let dir = shard_dir(dest_abs, fs_area, id, &mut created)?;
        let mut dst = create_new_file(&dir.join(fsutil::file_name(fs_area, id)))?;
        let digest = copy_hashed(&mut src, &mut dst, *size)?;
        if &digest != sha {
            return Err(Error::integrity(format!(
                "{} {id} does not match its recorded SHA-256",
                area.as_str()
            )));
        }
        sync_fd(&dst)?;
        let line = serde_json::to_string(&FileEntry {
            area: area.as_str().into(),
            id: id.to_hex(),
            size: *size,
            sha256: hex::encode(sha),
        })
        .map_err(|e| Error::other(e.to_string()))?;
        files.write_all(line.as_bytes())?;
        files.write_all(b"\n")?;
        files_hash.update(line.as_bytes());
        files_hash.update(b"\n");
        count += 1;
        bytes += size;
    }
    sync_fd(&files)?;
    let (db_bytes, db_sha) = hash_file(&db_dest)?;
    let manifest = Manifest {
        format: FORMAT.into(),
        store_id: meta.store_id.to_hex(),
        region: meta.region.clone(),
        storage_format: meta.format_version,
        created_at_ms: now_ms(),
        litebucket_version: env!("CARGO_PKG_VERSION").into(),
        sqlite_version: metadata::sqlite_version(),
        database_sha256: db_sha,
        database_bytes: db_bytes,
        files_sha256: hex::encode(files_hash.finalize()),
        file_count: count,
        file_bytes: bytes,
    };
    let mut mf = create_new_file(&dest_abs.join(MANIFEST))?;
    mf.write_all(
        serde_json::to_string_pretty(&manifest)
            .map_err(|e| Error::other(e.to_string()))?
            .as_bytes(),
    )?;
    sync_fd(&mf)?;
    sync_tree_dirs(&created, dest_abs)?;
    // Publish completion only after everything above is durable.
    let mut done = create_new_file(&dest_abs.join(COMPLETE))?;
    writeln!(done, "{FORMAT}")?;
    sync_fd(&done)?;
    fs::remove_file(dest_abs.join(INCOMPLETE))?;
    sync_dir(open_dir(dest_abs)?)?;
    let (plaintext_secrets, encrypted_secrets) = key_scheme_counts(&db_dest)?;
    Ok(Summary {
        destination: dest_abs.to_path_buf(),
        store_id: meta.store_id.to_string(),
        file_count: count,
        file_bytes: bytes,
        plaintext_secrets,
        encrypted_secrets,
    })
}

/// Offline backup: the server must be stopped.
pub fn backup(cfg: &Config, destination: &Path) -> Result<()> {
    let dest_abs = check_destination(&cfg.data_dir, destination)?;
    let (data, mut conn, meta, _) = open_offline(cfg, true)?;
    // Normalize interrupted operations without inventing committed objects.
    let recovery = with_write_tx(&mut conn, |tx| queries::recover(tx, now_ms()))?;
    if recovery.reopened_uploads + recovery.reclaimed_writing_blobs > 0 {
        println!(
            "recovered interrupted operations: {} uploads reopened, {} WRITING blobs reclaimed",
            recovery.reopened_uploads, recovery.reclaimed_writing_blobs
        );
    }
    begin(&dest_abs)?;
    snapshot(&conn, &dest_abs)?;
    finish(&data, &meta, &dest_abs)?.print();
    Ok(())
}

/// Online backup, run inside the server while it keeps serving. The pin keeps
/// garbage collection paused, so every file the snapshot references stays in
/// place until it is copied; uploads and deletes carry on meanwhile and are
/// in the backup only if they committed before the snapshot. The destination
/// is a path on the server's file system and must be absolute.
pub fn backup_online(store: &Store, _pin: &BackupPin, destination: &Path) -> Result<Summary> {
    if !destination.is_absolute() {
        return Err(Error::config(
            "an online backup destination must be an absolute path on the server",
        ));
    }
    let dest_abs = check_destination(&store.config.data_dir, destination)?;
    begin(&dest_abs)?;
    // VACUUM INTO only reads this database, in one read transaction, but
    // SQLite refuses it on a query_only (reader) connection.
    let conn = metadata::open_connection(&store.data.db_path(), metadata::Role::Writer, 5000)?;
    snapshot(&conn, &dest_abs)?;
    drop(conn);
    let summary = finish(&store.data, &store.meta, &dest_abs)?;
    tracing::info!(
        event = "backup_complete",
        destination = %summary.destination.display(),
        files = summary.file_count,
        bytes = summary.file_bytes,
        "online backup complete"
    );
    Ok(summary)
}

pub fn read_manifest(source: &Path) -> Result<Manifest> {
    if !source.join(COMPLETE).exists() || source.join(INCOMPLETE).exists() {
        return Err(Error::integrity(
            "backup is incomplete (missing BACKUP_COMPLETE marker)",
        ));
    }
    let text = fs::read_to_string(source.join(MANIFEST))?;
    let m: Manifest = serde_json::from_str(&text)
        .map_err(|e| Error::integrity(format!("invalid manifest: {e}")))?;
    if m.format != FORMAT {
        return Err(Error::integrity(format!(
            "unsupported backup format {}",
            m.format
        )));
    }
    if m.storage_format > migrations::FORMAT_VERSION {
        return Err(Error::integrity(
            "backup storage format is newer than this executable",
        ));
    }
    Ok(m)
}

/// (plaintext, encrypted) stored secrets in a database file; (0, 0) for
/// stores from before access keys moved into the database.
fn key_scheme_counts(db: &Path) -> Result<(i64, i64)> {
    let conn =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    if migrations::applied(&conn)?.iter().all(|(v, _)| *v < 3) {
        return Ok((0, 0));
    }
    queries::secret_scheme_counts(&conn)
}

/// Check that the backup's access keys can be decrypted before restoring.
fn check_keys(db: &Path, master_key_file: Option<&Path>, skip: bool) -> Result<()> {
    let (_, sealed) = key_scheme_counts(db)?;
    let Some(path) = master_key_file else {
        if sealed > 0 && !skip {
            return Err(Error::config(format!(
                "the backup contains {sealed} encrypted access key secret(s); pass --master-key-file to verify them (or --skip-key-check, then `litebucket admin recover --reset-keys`)"
            )));
        }
        return Ok(());
    };
    let codec = SecretCodec::encrypted(MasterKey::load(path)?);
    let conn =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let meta = queries::load_store_meta(&conn)?;
    crate::admin::load_credential_set(&conn, &codec, &meta.store_id, now_ms()).map_err(|e| {
        Error::config(format!(
            "the master key does not open the backup's access keys: {e}"
        ))
    })?;
    Ok(())
}

pub fn restore(
    source: &Path,
    data_dir: &Path,
    master_key_file: Option<&Path>,
    skip_key_check: bool,
) -> Result<()> {
    metadata::check_sqlite_runtime()?;
    let m = read_manifest(source)?;
    let (db_bytes, db_sha) = hash_file(&source.join(fsutil::DB_FILE))?;
    if db_bytes != m.database_bytes || db_sha != m.database_sha256 {
        return Err(Error::integrity(
            "backup database does not match the manifest",
        ));
    }
    check_keys(
        &source.join(fsutil::DB_FILE),
        master_key_file,
        skip_key_check,
    )?;
    // Verify the file list before writing anything.
    let mut h = Sha256::new();
    let mut entries = Vec::new();
    for line in BufReader::new(open_nofollow(&source.join(FILES))?).lines() {
        let line = line?;
        h.update(line.as_bytes());
        h.update(b"\n");
        let e: FileEntry = serde_json::from_str(&line)
            .map_err(|e| Error::integrity(format!("invalid file entry: {e}")))?;
        entries.push(e);
    }
    if hex::encode(h.finalize()) != m.files_sha256 || entries.len() as u64 != m.file_count {
        return Err(Error::integrity(
            "backup file list does not match the manifest",
        ));
    }

    let data = DataDir::create(data_dir)?;
    {
        let mut src = open_nofollow(&source.join(fsutil::DB_FILE))?;
        let mut dst = create_new_file(&data.db_path())?;
        let d = copy_hashed(&mut src, &mut dst, db_bytes)?;
        if hex::encode(d) != m.database_sha256 {
            return Err(Error::integrity("database changed while restoring"));
        }
        sync_fd(&dst)?;
        data.sync_root()?;
    }
    let mut conn = metadata::open_connection(&data.db_path(), metadata::Role::Writer, 5000)?;
    let ic: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    if ic != "ok" {
        return Err(Error::integrity(format!(
            "restored database integrity_check: {ic}"
        )));
    }
    migrations::verify(&conn)?;
    let meta = queries::load_store_meta(&conn)?;
    if meta.store_id.to_hex() != m.store_id || meta.region != m.region {
        return Err(Error::integrity(
            "restored database identity does not match the manifest",
        ));
    }
    let referenced = queries::referenced_blobs(&conn)?;
    let in_manifest: std::collections::HashSet<(String, String)> = entries
        .iter()
        .map(|e| (e.area.clone(), e.id.clone()))
        .collect();
    for (id, area, _, _) in &referenced {
        if !in_manifest.contains(&(area.as_str().to_string(), id.to_hex())) {
            return Err(Error::integrity(format!(
                "referenced {} {id} is missing from the backup",
                area.as_str()
            )));
        }
    }
    let mut dirs = std::collections::HashMap::new();
    for e in &entries {
        let area = area_of(&e.area)?;
        let id = StorageId::parse_hex(&e.id)
            .ok_or_else(|| Error::integrity("invalid storage id in backup"))?;
        let fs_area = area.fs_area();
        let rel = fsutil::relative_path(fs_area, &id);
        let mut src = open_nofollow(&source.join(&rel)).map_err(|err| {
            Error::integrity(format!("backup file {} missing: {err}", rel.display()))
        })?;
        let dir = data.shard_dir(fs_area, &id)?;
        let fd = rustix::fs::openat(
            &dir,
            fsutil::file_name(fs_area, &id).as_str(),
            rustix::fs::OFlags::WRONLY
                | rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::EXCL
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::from_raw_mode(0o600),
        )
        .map_err(std::io::Error::from)?;
        let mut dst = File::from(fd);
        let d = copy_hashed(&mut src, &mut dst, e.size)?;
        if hex::encode(d) != e.sha256 {
            return Err(Error::integrity(format!(
                "backup file {} is corrupt",
                rel.display()
            )));
        }
        sync_fd(&dst)?;
        dirs.insert(rel.parent().map(Path::to_path_buf), dir);
    }
    for d in dirs.values() {
        sync_dir(d)?;
    }
    // Same recovery and reference checks as startup.
    let r = with_write_tx(&mut conn, |tx| queries::recover(tx, now_ms()))?;
    let problems = queries::invariant_violations(&conn)?;
    if !problems.is_empty() {
        return Err(Error::integrity(format!(
            "restored store failed reference checks: {problems:?}"
        )));
    }
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
    drop(conn);
    data.sync_root()?;
    println!(
        "restore complete: {} files, store {} (region {}) at {}; reclaimed {} stale WRITING records",
        entries.len(),
        meta.store_id,
        meta.region,
        data_dir.display(),
        r.reclaimed_writing_blobs
    );
    println!(
        "note: point the configuration at this data directory and the store's master key before serving"
    );
    Ok(())
}
