//! The running store: shared state plus the durable blob lifecycle pipeline.
//!
//! Typestate keeps publication honest: a `WriteTicket` (registered WRITING
//! row) becomes a `ReceivedBlob` (bytes in staging, digests computed), then a
//! `StagedBlob` (synchronized), then a `PublishedBlob` (no-clobber renamed into
//! place with directory entries synchronized). Only a `PublishedBlob` can be
//! turned into commit facts. Dropping any of them before commit abandons the
//! WRITING row through a supervised task.

use std::collections::HashSet;
use std::fs::File;
use std::future::Future;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use bytes::{Bytes, BytesMut};
use tokio_util::task::TaskTracker;

use crate::capacity::{Capacity, Reservation};
use crate::checksums::{Algorithm, BodyHashes, Digests, StoredChecksum};
use crate::config::Config;
use crate::credentials::CredentialStore;
use crate::error::{Error, Result};
use crate::fsutil::{Area, DataDir};
use crate::ids::{BucketId, StorageId};
use crate::locks::KeyedLocks;
use crate::metadata::queries::{self, BlobArea, BlobFinal, StoreMeta};
use crate::metadata::{self, Db, Role, migrations, now_ms, with_named_write_tx, with_write_tx};
use crate::s3::error::{S3Error, S3Result};
use crate::telemetry::Metrics;

/// A source of decoded body bytes. Returning `Ok(None)` means the stream ended
/// and every framing/signature check performed by the source has passed.
pub trait ChunkSource: Send {
    fn next_chunk(&mut self) -> impl Future<Output = S3Result<Option<Bytes>>> + Send;
}

pub struct Store {
    pub config: Arc<Config>,
    pub data: Arc<DataDir>,
    pub db: Db,
    pub meta: StoreMeta,
    pub capacity: Capacity,
    pub key_locks: KeyedLocks<(BucketId, Vec<u8>)>,
    pub upload_locks: KeyedLocks<String>,
    active_blobs: Arc<Mutex<HashSet<StorageId>>>,
    active_uploads: Arc<Mutex<std::collections::HashMap<String, usize>>>,
    pub tracker: TaskTracker,
    halted: Mutex<Option<String>>,
    integrity_failed: AtomicBool,
    pub metrics: Arc<Metrics>,
    pub credentials: CredentialStore,
    ready: AtomicBool,
    pub startup: StartupReport,
    gauges: [std::sync::atomic::AtomicU64; 3],
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").field("data", &self.data).finish()
    }
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct StartupReport {
    pub sqlite_version: String,
    pub sqlite_source_id: String,
    pub migrations_applied: usize,
    pub recovery: queries::RecoveryReport,
}

pub async fn blocking<T, F>(f: F) -> io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> io::Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| io::Error::other(format!("blocking task failed: {e}")))?
}

/// Initialize a brand-new store (the explicit `init` flow).
pub fn initialize(config: &Config) -> Result<StoreMeta> {
    metadata::check_sqlite_runtime()?;
    let data = DataDir::create(&config.data_dir)?;
    let conn = metadata::create_database(&data.db_path())?;
    migrations::apply(&conn)?;
    let tx = rusqlite::Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate)?;
    let meta = queries::init_store_meta(&tx, &config.region, now_ms())?;
    tx.commit()?;
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
    drop(conn);
    data.sync_root()?;
    Ok(meta)
}

/// Open a locked data directory and its metadata for offline maintenance or
/// serving: verifies format/region, applies compatible migrations (when
/// `migrate`), checks integrity. Does not run recovery.
pub fn open_offline(config: &Config, migrate: bool) -> Result<(DataDir, rusqlite::Connection, StoreMeta, usize)> {
    metadata::check_sqlite_runtime()?;
    let data = DataDir::open(&config.data_dir)?;
    let db_path = data.db_path();
    if !db_path.exists() {
        return Err(Error::config(format!(
            "{} has no metadata database; refusing to create an empty store (run `storlite init` for a new store)",
            config.data_dir.display()
        )));
    }
    let conn = metadata::open_connection(&db_path, Role::Writer, config.database.busy_timeout_ms)?;
    let check: String = conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
    if check != "ok" {
        return Err(Error::integrity(format!("metadata quick_check failed: {check}")));
    }
    let applied = if migrate {
        migrations::apply(&conn)?
    } else {
        migrations::verify(&conn)?;
        0
    };
    let meta = queries::load_store_meta(&conn)?;
    if meta.format_version > migrations::FORMAT_VERSION {
        return Err(Error::config(format!(
            "store format {} is newer than this executable ({})",
            meta.format_version,
            migrations::FORMAT_VERSION
        )));
    }
    if meta.region != config.region {
        return Err(Error::config(format!(
            "configured region {} conflicts with the store's immutable region {}",
            config.region, meta.region
        )));
    }
    Ok((data, conn, meta, applied))
}

impl Store {
    /// Open, recover, and start workers. The S3 listener must not be bound
    /// before this succeeds.
    pub fn open(config: Config, credentials: CredentialStore) -> Result<Arc<Self>> {
        let (data, conn, meta, applied) = open_offline(&config, true)?;
        let mut conn = conn;
        let recovery = with_write_tx(&mut conn, |tx| queries::recover(tx, now_ms()))?;
        if recovery.reopened_uploads + recovery.reclaimed_writing_blobs > 0 {
            tracing::warn!(
                event = "recovery",
                reopened_uploads = recovery.reopened_uploads,
                reclaimed_writing_blobs = recovery.reclaimed_writing_blobs,
                "recovered interrupted operations"
            );
        }
        let part_bytes = queries::committed_part_bytes(&conn)?;
        drop(conn);
        let data = Arc::new(data);
        let db = Db::start(&data.db_path(), &config.database)?;
        let capacity = Capacity::new(&config.limits, data.clone(), part_bytes);
        let metrics = Arc::new(Metrics::default());
        metrics.recovery_actions.fetch_add(
            (recovery.reopened_uploads + recovery.reclaimed_writing_blobs) as u64,
            Ordering::Relaxed,
        );
        Ok(Arc::new(Self {
            config: Arc::new(config),
            data,
            db,
            meta,
            capacity,
            key_locks: KeyedLocks::default(),
            upload_locks: KeyedLocks::default(),
            active_blobs: Arc::default(),
            active_uploads: Arc::default(),
            tracker: TaskTracker::new(),
            halted: Mutex::new(None),
            integrity_failed: AtomicBool::new(false),
            metrics,
            credentials,
            ready: AtomicBool::new(false),
            startup: StartupReport {
                sqlite_version: metadata::sqlite_version(),
                sqlite_source_id: metadata::sqlite_source_id(),
                migrations_applied: applied,
                recovery,
            },
            gauges: Default::default(),
        }))
    }

    /// (garbage blobs, garbage bytes, active multipart uploads), refreshed by maintenance.
    pub fn maintenance_gauges(&self) -> (u64, u64, u64) {
        (
            self.gauges[0].load(Ordering::Relaxed),
            self.gauges[1].load(Ordering::Relaxed),
            self.gauges[2].load(Ordering::Relaxed),
        )
    }

    pub fn set_maintenance_gauges(&self, blobs: u64, bytes: u64, uploads: u64) {
        self.gauges[0].store(blobs, Ordering::Relaxed);
        self.gauges[1].store(bytes, Ordering::Relaxed);
        self.gauges[2].store(uploads, Ordering::Relaxed);
    }

    pub fn set_ready(&self, ready: bool) {
        self.ready.store(ready, Ordering::SeqCst);
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::SeqCst)
    }

    pub fn garbage_after(&self) -> i64 {
        now_ms().saturating_add((self.config.maintenance.garbage_grace_seconds as i64).saturating_mul(1000))
    }

    /// Stop accepting mutations and automatic cleanup until an operator
    /// restarts and recovery succeeds.
    pub fn halt(&self, reason: impl Into<String>) {
        let reason = reason.into();
        tracing::error!(event = "mutations_halted", %reason, "halting mutations pending recovery");
        let mut h = self.halted.lock().unwrap_or_else(|e| e.into_inner());
        if h.is_none() {
            *h = Some(reason);
        }
    }

    pub fn halted_reason(&self) -> Option<String> {
        self.halted.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn check_writable(&self) -> S3Result<()> {
        match self.halted_reason() {
            Some(r) => Err(Error::Halted(r).into()),
            None => Ok(()),
        }
    }

    /// Record a storage-integrity fault (missing/corrupt referenced file, etc.).
    pub fn integrity_fault(&self, what: &str) {
        self.integrity_failed.store(true, Ordering::SeqCst);
        self.metrics.integrity_errors.fetch_add(1, Ordering::Relaxed);
        tracing::error!(event = "integrity_failure", detail = %what, "storage integrity failure");
    }

    pub fn integrity_failed(&self) -> bool {
        self.integrity_failed.load(Ordering::SeqCst)
    }

    pub fn is_blob_active(&self, id: &StorageId) -> bool {
        self.active_blobs.lock().unwrap_or_else(|e| e.into_inner()).contains(id)
    }

    /// Mark an upload as having in-flight work (protects it from expiry).
    pub fn upload_activity(&self, upload_id: &str) -> UploadActivity {
        *self
            .active_uploads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(upload_id.to_string())
            .or_default() += 1;
        UploadActivity {
            id: upload_id.to_string(),
            map: self.active_uploads.clone(),
        }
    }

    pub fn is_upload_active(&self, upload_id: &str) -> bool {
        self.active_uploads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(upload_id)
    }

    /// Run work that must reach a known outcome even if the requester goes away.
    pub async fn supervise<T, F>(&self, fut: F) -> S3Result<T>
    where
        T: Send + 'static,
        F: Future<Output = S3Result<T>> + Send + 'static,
    {
        self.tracker
            .spawn(fut)
            .await
            .map_err(|e| S3Error::internal().with_detail(format!("supervised task failed: {e}")))?
    }

    /// Convert an I/O failure. EIO from the storage stack is treated as an
    /// unreconciled synchronization fault: mutations halt until restart.
    pub fn io_failure(&self, e: io::Error) -> S3Error {
        if e.raw_os_error() == Some(rustix::io::Errno::IO.raw_os_error()) {
            self.halt(format!("storage I/O error: {e}"));
        }
        Error::from(e).into()
    }

    /// Allocate and register a new WRITING blob, avoiding tracked and
    /// untracked ID collisions without touching existing files.
    pub async fn new_blob(self: &Arc<Self>, area: BlobArea) -> S3Result<WriteTicket> {
        self.check_writable()?;
        for _ in 0..8 {
            let id = StorageId::allocate();
            let data = self.data.clone();
            let fs_area = area.fs_area();
            let occupied = blocking(move || {
                Ok(data.path_exists(Area::Staging, &id)? || data.path_exists(fs_area, &id)?)
            })
            .await
            .map_err(Error::from)?;
            if occupied {
                tracing::warn!(event = "id_collision", storage_id = %id, "untracked file occupies a fresh storage ID; skipping");
                continue;
            }
            let now = now_ms();
            let registered = self
                .db
                .write(move |c| with_named_write_tx(c, "register", |tx| queries::register_blob(tx, &id, area, now)))
                .await?;
            if !registered {
                tracing::warn!(event = "id_collision", storage_id = %id, "tracked storage ID collision; retrying");
                continue;
            }
            let guard = ActiveGuard::new(id, self.active_blobs.clone());
            return Ok(WriteTicket {
                store: self.clone(),
                id,
                area,
                guard,
                armed: true,
            });
        }
        Err(S3Error::internal().with_detail("could not allocate a storage ID"))
    }

    /// Mark a WRITING blob garbage after a definite pre-commit failure.
    pub async fn abandon(&self, id: StorageId) {
        let after = self.garbage_after();
        let res = self
            .db
            .write(move |c| with_write_tx(c, |tx| queries::abandon_blob(tx, &id, after)))
            .await;
        if let Err(e) = res {
            // The row stays WRITING; restart recovery reclaims it.
            tracing::warn!(error = %e, storage_id = %id, "could not mark abandoned blob as garbage");
        }
    }

    /// Stream a body into a new exclusively created staging file while
    /// computing MD5, internal SHA-256, and requested S3 checksums.
    pub async fn receive<S: ChunkSource>(
        self: &Arc<Self>,
        ticket: WriteTicket,
        src: &mut S,
        algorithms: &[Algorithm],
        max_len: u64,
        mut reservation: Option<&mut Reservation>,
    ) -> S3Result<ReceivedBlob> {
        let data = self.data.clone();
        let id = ticket.id;
        let file = match blocking(move || data.create_staging(&id)).await {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                return Err(self.quarantine(ticket, "staging path appeared after registration").await);
            }
            Err(e) => return Err(self.io_failure(e)),
        };
        let buf_target = self.config.limits.transfer_buffer_bytes;
        let mut state = Some((file, BodyHashes::new(algorithms), BytesMut::with_capacity(buf_target)));
        let mut total: u64 = 0;
        loop {
            let chunk = src.next_chunk().await?;
            let done = chunk.is_none();
            let (file, hashes, mut buf) = state.take().expect("writer state");
            if let Some(chunk) = chunk {
                total = total.saturating_add(chunk.len() as u64);
                if total > max_len {
                    return Err(S3Error::entity_too_large());
                }
                if let Some(r) = reservation.as_deref_mut() {
                    self.capacity.grow(r, total)?;
                }
                buf.extend_from_slice(&chunk);
            }
            if buf.len() >= buf_target || (done && !buf.is_empty()) {
                let keep = ticket.guard.clone();
                let (file, hashes, mut buf) = blocking(move || {
                    let _keep = keep;
                    let mut file = file;
                    let mut hashes = hashes;
                    crate::failpoint::io("write")?;
                    hashes.update(&buf);
                    file.write_all(&buf)?;
                    Ok((file, hashes, buf))
                })
                .await
                .map_err(|e| self.io_failure(e))?;
                buf.clear();
                state = Some((file, hashes, buf));
            } else {
                state = Some((file, hashes, buf));
            }
            if done {
                break;
            }
        }
        let (file, hashes, _) = state.take().expect("writer state");
        self.metrics.bytes_received.fetch_add(total, Ordering::Relaxed);
        crate::failpoint::hit("after_body_received");
        Ok(ReceivedBlob {
            ticket,
            file,
            digests: hashes.finish(),
        })
    }

    /// Re-wrap a WRITING blob registered by another transaction (multipart
    /// completion registers its output while entering COMPLETING).
    pub fn adopt_blob(self: &Arc<Self>, id: StorageId, area: BlobArea) -> WriteTicket {
        WriteTicket {
            store: self.clone(),
            id,
            area,
            guard: ActiveGuard::new(id, self.active_blobs.clone()),
            armed: true,
        }
    }

    /// Concatenate immutable source files into a new staging file (CopyObject
    /// and multipart assembly). Tracked sources are opened one at a time so
    /// at most one input descriptor is held, and their sizes (and MD5s, when
    /// given) are verified against metadata.
    pub async fn copy_into(
        self: &Arc<Self>,
        ticket: WriteTicket,
        sources: Vec<CopySource>,
        algorithms: &[Algorithm],
    ) -> S3Result<ReceivedBlob> {
        let data = self.data.clone();
        let id = ticket.id;
        let file = match blocking(move || data.create_staging(&id)).await {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                return Err(self.quarantine(ticket, "staging path appeared after registration").await);
            }
            Err(e) => return Err(self.io_failure(e)),
        };
        let buf_size = self.config.limits.transfer_buffer_bytes;
        let algs = algorithms.to_vec();
        let keep = ticket.guard.clone();
        let data = self.data.clone();
        let (file, digests) = blocking(move || {
            let _keep = keep;
            let mut out = file;
            let mut hashes = BodyHashes::new(&algs);
            let mut buf = vec![0u8; buf_size];
            for source in sources {
                let (mut src, expected, md5) = match source {
                    CopySource::File(f, size) => (f, size, None),
                    CopySource::Tracked { area, id, size, md5 } => (data.open_read(area, &id)?, size, md5),
                };
                if crate::fsutil::file_len(&src)? != expected {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "source file size differs from metadata"));
                }
                let mut part_md5 = md5.map(|_| <md5::Md5 as sha2::Digest>::new());
                let mut remaining = expected;
                while remaining > 0 {
                    let want = remaining.min(buf.len() as u64) as usize;
                    let n = io::Read::read(&mut src, &mut buf[..want])?;
                    if n == 0 {
                        return Err(io::Error::new(io::ErrorKind::InvalidData, "source file shorter than recorded size"));
                    }
                    crate::failpoint::io("write")?;
                    hashes.update(&buf[..n]);
                    if let Some(h) = part_md5.as_mut() {
                        sha2::Digest::update(h, &buf[..n]);
                    }
                    out.write_all(&buf[..n])?;
                    remaining -= n as u64;
                }
                if let (Some(h), Some(want)) = (part_md5, md5) {
                    let got: [u8; 16] = sha2::Digest::finalize(h).into();
                    if got != want {
                        return Err(io::Error::new(io::ErrorKind::InvalidData, "source file content differs from its recorded MD5"));
                    }
                }
            }
            Ok((out, hashes.finish()))
        })
        .await
        .map_err(|e| {
            if e.kind() == io::ErrorKind::InvalidData || e.kind() == io::ErrorKind::NotFound {
                self.integrity_fault(&format!("copy/assembly input: {e}"));
            }
            self.io_failure(e)
        })?;
        Ok(ReceivedBlob { ticket, file, digests })
    }

    /// Late collision: preserve the preexisting path, remove only our own
    /// tracking so cleanup never touches the other file.
    async fn quarantine(&self, mut ticket: WriteTicket, why: &str) -> S3Error {
        ticket.armed = false;
        let id = ticket.id;
        self.integrity_fault(&format!("storage ID {id}: {why}"));
        let _ = self
            .db
            .write(move |c| {
                with_write_tx(c, |tx| {
                    tx.execute(
                        "DELETE FROM blobs WHERE storage_id = ?1 AND state = 'writing'",
                        [id.as_bytes().as_slice()],
                    )?;
                    Ok(())
                })
            })
            .await;
        S3Error::internal().with_detail(format!("storage ID collision: {why}"))
    }

    /// Reconcile an uncertain commit by re-reading the blob's state.
    pub async fn reconcile(&self, id: StorageId) -> Reconciled {
        match self.db.write(move |c| queries::blob_state(c, &id)).await {
            Ok(Some(s)) if s == "ready" => Reconciled::Committed,
            Ok(Some(s)) if s == "writing" => Reconciled::NotCommitted,
            Ok(_) => Reconciled::Unknown,
            Err(_) => Reconciled::Unknown,
        }
    }
}

/// An input to `copy_into`.
pub enum CopySource {
    /// An already opened immutable file (CopyObject opens under its key guard).
    File(File, u64),
    /// A tracked file opened lazily, optionally verified against its MD5.
    Tracked {
        area: Area,
        id: StorageId,
        size: u64,
        md5: Option<[u8; 16]>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reconciled {
    Committed,
    NotCommitted,
    Unknown,
}

/// Keeps a storage ID in the in-process active set while any clone lives.
#[derive(Clone)]
pub struct ActiveGuard(#[allow(dead_code)] Arc<ActiveInner>);

struct ActiveInner {
    id: StorageId,
    set: Arc<Mutex<HashSet<StorageId>>>,
}

impl ActiveGuard {
    fn new(id: StorageId, set: Arc<Mutex<HashSet<StorageId>>>) -> Self {
        set.lock().unwrap_or_else(|e| e.into_inner()).insert(id);
        Self(Arc::new(ActiveInner { id, set }))
    }
}

impl Drop for ActiveInner {
    fn drop(&mut self) {
        self.set.lock().unwrap_or_else(|e| e.into_inner()).remove(&self.id);
    }
}

pub struct UploadActivity {
    id: String,
    map: Arc<Mutex<std::collections::HashMap<String, usize>>>,
}

impl Drop for UploadActivity {
    fn drop(&mut self) {
        let mut m = self.map.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = m.get_mut(&self.id) {
            *n -= 1;
            if *n == 0 {
                m.remove(&self.id);
            }
        }
    }
}

/// A registered WRITING blob owned by one operation.
pub struct WriteTicket {
    store: Arc<Store>,
    id: StorageId,
    area: BlobArea,
    guard: ActiveGuard,
    armed: bool,
}

impl WriteTicket {
    pub fn id(&self) -> StorageId {
        self.id
    }

    pub fn area(&self) -> BlobArea {
        self.area
    }

    /// The commit transaction made this blob READY (or reconciliation proved
    /// it): nothing to abandon.
    pub fn committed(mut self) {
        self.armed = false;
    }

    /// The outcome is unknown and could not be reconciled: leave the row
    /// WRITING for restart recovery and never delete the file.
    pub fn leave_for_recovery(mut self) {
        self.armed = false;
    }
}

impl Drop for WriteTicket {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let store = self.store.clone();
        let id = self.id;
        let guard = self.guard.clone();
        // Supervised cleanup; the guard keeps GC away until it finishes and
        // until any in-flight blocking write holding a clone completes.
        if tokio::runtime::Handle::try_current().is_ok() {
            store.tracker.clone().spawn(async move {
                store.abandon(id).await;
                drop(guard);
            });
        }
    }
}

/// Bytes are in staging and digests are known; not yet synchronized.
pub struct ReceivedBlob {
    ticket: WriteTicket,
    file: File,
    pub digests: Digests,
}

impl ReceivedBlob {
    pub fn id(&self) -> StorageId {
        self.ticket.id
    }

    /// Flush and synchronize the staging file contents.
    pub async fn sync(self) -> S3Result<StagedBlob> {
        let ReceivedBlob { ticket, file, digests } = self;
        let keep = ticket.guard.clone();
        let store = ticket.store.clone();
        let res = blocking(move || {
            let _keep = keep;
            let mut f = file;
            f.flush()?;
            crate::fsutil::sync_fd(&f)
        })
        .await;
        if let Err(e) = res {
            return Err(store.io_failure(e));
        }
        crate::failpoint::hit("after_file_sync");
        Ok(StagedBlob { ticket, digests })
    }
}

/// Synchronized staging file awaiting publication.
pub struct StagedBlob {
    ticket: WriteTicket,
    pub digests: Digests,
}

impl StagedBlob {
    pub fn id(&self) -> StorageId {
        self.ticket.id
    }

    /// No-clobber publication to the final sharded path plus directory sync.
    pub async fn publish(self) -> S3Result<PublishedBlob> {
        let StagedBlob { ticket, digests } = self;
        let store = ticket.store.clone();
        let data = store.data.clone();
        let id = ticket.id;
        let area = ticket.area.fs_area();
        let keep = ticket.guard.clone();
        match blocking(move || {
            let _keep = keep;
            data.publish(area, &id)
        })
        .await
        {
            Ok(()) => Ok(PublishedBlob { ticket, digests }),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                // Remove only our own staging file, then drop our tracking.
                let data = store.data.clone();
                let _ = blocking(move || data.remove(Area::Staging, &id).map(|_| ())).await;
                Err(store.quarantine(ticket, "final path already occupied at publication").await)
            }
            Err(e) => Err(store.io_failure(e)),
        }
    }
}

/// A durable, published file whose WRITING row awaits its commit transaction.
pub struct PublishedBlob {
    ticket: WriteTicket,
    pub digests: Digests,
}

impl PublishedBlob {
    pub fn id(&self) -> StorageId {
        self.ticket.id
    }

    pub fn final_facts(&self, checksum: Option<StoredChecksum>) -> BlobFinal {
        BlobFinal {
            storage_id: self.ticket.id,
            size: self.digests.len,
            md5: self.digests.md5,
            sha256: self.digests.sha256,
            checksum,
        }
    }

    pub fn into_ticket(self) -> WriteTicket {
        self.ticket
    }
}
