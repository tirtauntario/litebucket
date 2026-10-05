//! Embedded SQLite metadata: connection policy, migrations, and bounded workers.
//!
//! One dedicated writer thread and N reader threads each own a connection and
//! drain a bounded queue. Requests that cannot enqueue within the configured
//! wait receive a retryable overload error.

pub mod migrations;
pub mod queries;

use std::cell::Cell;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags, TransactionBehavior};
use tokio::sync::{mpsc, oneshot};

use crate::config::DatabaseConfig;
use crate::error::{Error, Result};

/// Minimum bundled SQLite release: contains the WAL-reset corruption fix.
pub const MIN_SQLITE_VERSION: (u32, u32, u32) = (3, 51, 3);

pub fn sqlite_version() -> String {
    rusqlite::version().to_string()
}

pub fn sqlite_source_id() -> String {
    Connection::open_in_memory()
        .and_then(|c| c.query_row("SELECT sqlite_source_id()", [], |r| r.get(0)))
        .unwrap_or_else(|_| "unknown".into())
}

/// Verify the linked SQLite runtime meets the minimum patched release.
pub fn check_sqlite_runtime() -> Result<()> {
    let n = rusqlite::version_number() as u32;
    let v = (n / 1_000_000, (n / 1000) % 1000, n % 1000);
    if v < MIN_SQLITE_VERSION {
        return Err(Error::config(format!(
            "linked SQLite {} is older than the required patched release {}.{}.{}",
            sqlite_version(),
            MIN_SQLITE_VERSION.0,
            MIN_SQLITE_VERSION.1,
            MIN_SQLITE_VERSION.2
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Writer,
    Reader,
}

/// Open a connection with the mandatory durability settings and verify them.
pub fn open_connection(path: &Path, role: Role, busy_timeout_ms: u64) -> Result<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_URI;
    let conn = Connection::open_with_flags(path, flags)?;
    configure(&conn, role, busy_timeout_ms)?;
    Ok(conn)
}

/// Create a brand-new database file (only for `init`).
pub fn create_database(path: &Path) -> Result<Connection> {
    if path.exists() {
        return Err(Error::config(format!("{} already exists", path.display())));
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_CREATE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_URI;
    let conn = Connection::open_with_flags(path, flags)?;
    configure(&conn, Role::Writer, 5000)?;
    Ok(conn)
}

fn configure(conn: &Connection, role: Role, busy_timeout_ms: u64) -> Result<()> {
    conn.busy_timeout(Duration::from_millis(busy_timeout_ms))?;
    let mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(Error::config(format!(
            "SQLite refused WAL mode (got {mode})"
        )));
    }
    conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL;")?;
    #[cfg(target_vendor = "apple")]
    conn.execute_batch("PRAGMA fullfsync=ON; PRAGMA checkpoint_fullfsync=ON;")?;
    if role == Role::Reader {
        conn.execute_batch("PRAGMA query_only=ON;")?;
    }
    verify_settings(conn)?;
    Ok(())
}

/// Effective connection settings, used by tests and `doctor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub journal_mode: String,
    pub synchronous: i64,
    pub foreign_keys: i64,
}

pub fn read_settings(conn: &Connection) -> Result<Settings> {
    Ok(Settings {
        journal_mode: conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))?,
        synchronous: conn.query_row("PRAGMA synchronous", [], |r| r.get(0))?,
        foreign_keys: conn.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?,
    })
}

fn verify_settings(conn: &Connection) -> Result<()> {
    let s = read_settings(conn)?;
    if !s.journal_mode.eq_ignore_ascii_case("wal") || s.synchronous != 2 || s.foreign_keys != 1 {
        return Err(Error::config(format!(
            "SQLite durability settings not applied: {s:?}"
        )));
    }
    Ok(())
}

thread_local! {
    /// Set when a commit failed with an outcome that may be unknown; the
    /// worker reopens its connection before taking more work.
    static NEEDS_RECONNECT: Cell<bool> = const { Cell::new(false) };
}

/// Run `f` in a `BEGIN IMMEDIATE` transaction and commit it. A commit failure
/// other than a definite busy/locked rejection is reported as
/// `CommitUncertain` and the connection is recycled.
pub fn with_write_tx<T>(
    conn: &mut Connection,
    f: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T>,
) -> Result<T> {
    with_named_write_tx(conn, "", f)
}

/// Like `with_write_tx`; `name` labels the transition for test failpoints
/// (`before_commit:<name>`, `after_commit:<name>`, `commit:<name>`).
pub fn with_named_write_tx<T>(
    conn: &mut Connection,
    name: &'static str,
    f: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T>,
) -> Result<T> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let value = f(&tx)?;
    let named = !name.is_empty();
    if named {
        crate::failpoint::hit(&format!("before_commit:{name}"));
        if let Err(e) = crate::failpoint::io(&format!("commit:{name}")) {
            NEEDS_RECONNECT.with(|c| c.set(true));
            tracing::error!(error = %e, "injected commit failure");
            return Err(Error::CommitUncertain);
        }
    }
    match tx.commit() {
        Ok(()) => {
            if named {
                crate::failpoint::hit(&format!("after_commit:{name}"));
            }
            Ok(value)
        }
        Err(e) => {
            let definite = matches!(
                e.sqlite_error_code(),
                Some(rusqlite::ErrorCode::DatabaseBusy) | Some(rusqlite::ErrorCode::DatabaseLocked)
            );
            if definite {
                Err(e.into())
            } else {
                NEEDS_RECONNECT.with(|c| c.set(true));
                tracing::error!(error = %e, "metadata commit failed with unknown outcome");
                Err(Error::CommitUncertain)
            }
        }
    }
}

/// Run `f` inside a deferred read transaction: one consistent snapshot.
pub fn with_read_tx<T>(
    conn: &mut Connection,
    f: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T>,
) -> Result<T> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Deferred)?;
    let v = f(&tx)?;
    tx.finish()?;
    Ok(v)
}

type Job = Box<dyn FnOnce(&mut Connection) + Send>;

/// Delivers a batched job's result once the shared commit outcome is known.
type Finisher = Box<dyn FnOnce(CommitStatus) + Send>;

/// A write transaction body. It runs inside its own savepoint of a shared
/// group-commit transaction (or receives `None` if the transaction could not
/// begin) and reports whether it succeeded.
type TxBody = Box<dyn FnOnce(Option<&rusqlite::Transaction<'_>>) -> (bool, Finisher) + Send>;

/// Maximum write transactions combined into one durable commit.
const MAX_GROUP_COMMIT: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommitStatus {
    Committed,
    /// Definitely not committed (busy/locked or could not begin).
    NotCommitted,
    /// The commit may or may not have happened.
    Uncertain,
}

enum Msg {
    Job(Job),
    Tx(&'static str, TxBody),
    Stop,
}

#[derive(Debug, Default)]
pub struct DbStats {
    pub write_jobs: AtomicU64,
    pub read_jobs: AtomicU64,
    pub write_queue_depth: AtomicU64,
    pub read_queue_depth: AtomicU64,
    pub queue_rejections: AtomicU64,
    pub write_micros_total: AtomicU64,
    pub commit_uncertain: AtomicU64,
}

struct Inner {
    writer: mpsc::Sender<Msg>,
    readers: Vec<mpsc::Sender<Msg>>,
    next_reader: AtomicUsize,
    queue_wait: Duration,
    threads: Mutex<Vec<JoinHandle<()>>>,
    stats: Arc<DbStats>,
    path: PathBuf,
}

/// Handle to the metadata workers. Cheap to clone.
#[derive(Clone)]
pub struct Db {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Db")
            .field("path", &self.inner.path)
            .finish()
    }
}

impl Db {
    pub fn start(path: &Path, cfg: &DatabaseConfig) -> Result<Self> {
        let stats = Arc::new(DbStats::default());
        let mut threads = Vec::new();
        let writer_conn = open_connection(path, Role::Writer, cfg.busy_timeout_ms)?;
        let (wtx, wrx) = mpsc::channel(cfg.writer_queue_capacity);
        threads.push(spawn_worker(
            "litebucket-db-writer",
            writer_conn,
            wrx,
            path.to_path_buf(),
            Role::Writer,
            cfg.busy_timeout_ms,
            stats.clone(),
        )?);
        let mut readers = Vec::new();
        for i in 0..cfg.reader_connections {
            let conn = open_connection(path, Role::Reader, cfg.busy_timeout_ms)?;
            let (tx, rx) = mpsc::channel(cfg.reader_queue_capacity);
            threads.push(spawn_worker(
                &format!("litebucket-db-reader-{i}"),
                conn,
                rx,
                path.to_path_buf(),
                Role::Reader,
                cfg.busy_timeout_ms,
                stats.clone(),
            )?);
            readers.push(tx);
        }
        Ok(Self {
            inner: Arc::new(Inner {
                writer: wtx,
                readers,
                next_reader: AtomicUsize::new(0),
                queue_wait: Duration::from_millis(cfg.queue_wait_ms),
                threads: Mutex::new(threads),
                stats,
                path: path.to_path_buf(),
            }),
        })
    }

    pub fn stats(&self) -> &DbStats {
        &self.inner.stats
    }

    /// Run a job on the writer connection. The job runs to completion even if
    /// the caller stops waiting; callers must not infer rollback from a drop.
    pub async fn write<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let stats = self.inner.stats.clone();
        stats.write_jobs.fetch_add(1, Ordering::Relaxed);
        let depth = &stats.write_queue_depth;
        let res = submit(&self.inner.writer, self.inner.queue_wait, depth, &stats, f).await;
        if matches!(res, Err(Error::CommitUncertain)) {
            stats.commit_uncertain.fetch_add(1, Ordering::Relaxed);
        }
        res
    }

    /// Run `f` as a write transaction. Concurrent callers are group-committed:
    /// the writer combines queued transactions into one `BEGIN IMMEDIATE`
    /// transaction with a savepoint per caller, so one durable commit (one
    /// WAL sync) serves the whole batch. A caller's error rolls back only its
    /// own savepoint. Results are delivered only after the shared commit, so
    /// success always means durably committed. `name` labels the transition
    /// for test failpoints.
    pub async fn write_tx<T, F>(&self, name: &'static str, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&rusqlite::Transaction<'_>) -> Result<T> + Send + 'static,
    {
        let stats = self.inner.stats.clone();
        stats.write_jobs.fetch_add(1, Ordering::Relaxed);
        let (rtx, rrx) = oneshot::channel::<Result<T>>();
        let body: TxBody = Box::new(move |tx| {
            let r = match tx {
                Some(tx) => std::panic::catch_unwind(AssertUnwindSafe(|| f(tx)))
                    .unwrap_or_else(|_| Err(Error::other("metadata job panicked"))),
                None => Err(Error::Overloaded("metadata transaction could not begin")),
            };
            let ok = r.is_ok();
            let fin: Finisher = Box::new(move |status| {
                let out = match (r, status) {
                    (Err(e), _) => Err(e),
                    (Ok(v), CommitStatus::Committed) => Ok(v),
                    (Ok(_), CommitStatus::NotCommitted) => {
                        Err(Error::Overloaded("metadata database busy"))
                    }
                    (Ok(_), CommitStatus::Uncertain) => Err(Error::CommitUncertain),
                };
                let _ = rtx.send(out);
            });
            (ok, fin)
        });
        let permit =
            match tokio::time::timeout(self.inner.queue_wait, self.inner.writer.reserve()).await {
                Ok(Ok(p)) => p,
                Ok(Err(_)) => return Err(Error::other("metadata worker stopped")),
                Err(_) => {
                    stats.queue_rejections.fetch_add(1, Ordering::Relaxed);
                    return Err(Error::Overloaded("metadata queue full"));
                }
            };
        stats.write_queue_depth.fetch_add(1, Ordering::Relaxed);
        permit.send(Msg::Tx(name, body));
        let res = rrx
            .await
            .map_err(|_| Error::other("metadata worker dropped a job"))?;
        if matches!(res, Err(Error::CommitUncertain)) {
            stats.commit_uncertain.fetch_add(1, Ordering::Relaxed);
        }
        res
    }

    /// Run a job on a read-only connection.
    pub async fn read<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let stats = self.inner.stats.clone();
        stats.read_jobs.fetch_add(1, Ordering::Relaxed);
        let i = self.inner.next_reader.fetch_add(1, Ordering::Relaxed) % self.inner.readers.len();
        submit(
            &self.inner.readers[i],
            self.inner.queue_wait,
            &stats.read_queue_depth,
            &stats,
            f,
        )
        .await
    }

    /// Stop all workers after their queues drain. The writer performs a
    /// truncating checkpoint before closing.
    pub fn shutdown(&self) {
        let _ = self.inner.writer.try_send(Msg::Stop);
        for r in &self.inner.readers {
            let _ = r.try_send(Msg::Stop);
        }
        let threads =
            std::mem::take(&mut *self.inner.threads.lock().unwrap_or_else(|e| e.into_inner()));
        for t in threads {
            let _ = t.join();
        }
    }
}

async fn submit<T, F>(
    tx: &mpsc::Sender<Msg>,
    wait: Duration,
    depth: &AtomicU64,
    stats: &DbStats,
    f: F,
) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
{
    let (rtx, rrx) = oneshot::channel();
    let job: Job = Box::new(move |conn| {
        let r = std::panic::catch_unwind(AssertUnwindSafe(|| f(conn)))
            .unwrap_or_else(|_| Err(Error::other("metadata job panicked")));
        let _ = rtx.send(r);
    });
    let permit = match tokio::time::timeout(wait, tx.reserve()).await {
        Ok(Ok(p)) => p,
        Ok(Err(_)) => return Err(Error::other("metadata worker stopped")),
        Err(_) => {
            stats.queue_rejections.fetch_add(1, Ordering::Relaxed);
            return Err(Error::Overloaded("metadata queue full"));
        }
    };
    depth.fetch_add(1, Ordering::Relaxed);
    permit.send(Msg::Job(job));
    rrx.await
        .map_err(|_| Error::other("metadata worker dropped a job"))?
}

/// Execute a batch of write transactions as one group commit.
fn run_group(conn: &mut Connection, batch: Vec<(&'static str, TxBody)>) {
    let mut names: Vec<&'static str> = batch
        .iter()
        .map(|(n, _)| *n)
        .filter(|n| !n.is_empty())
        .collect();
    names.dedup();
    let tx = match conn.transaction_with_behavior(TransactionBehavior::Immediate) {
        Ok(tx) => tx,
        Err(e) => {
            tracing::warn!(error = %e, "could not begin metadata write transaction");
            for (_, body) in batch {
                let (_, fin) = body(None);
                fin(CommitStatus::NotCommitted);
            }
            return;
        }
    };
    let mut finishers = Vec::with_capacity(batch.len());
    let mut broken = false;
    for (_, body) in batch {
        if broken {
            let (_, fin) = body(None);
            finishers.push(fin);
            continue;
        }
        if tx.execute_batch("SAVEPOINT litebucket_job").is_err() {
            broken = true;
            let (_, fin) = body(None);
            finishers.push(fin);
            continue;
        }
        let (ok, fin) = body(Some(&tx));
        let end = if ok {
            "RELEASE litebucket_job"
        } else {
            "ROLLBACK TO litebucket_job; RELEASE litebucket_job"
        };
        if tx.execute_batch(end).is_err() {
            broken = true;
        }
        finishers.push(fin);
    }
    if broken {
        drop(tx);
        for fin in finishers {
            fin(CommitStatus::NotCommitted);
        }
        return;
    }
    for n in &names {
        crate::failpoint::hit(&format!("before_commit:{n}"));
    }
    let injected = names
        .iter()
        .any(|n| crate::failpoint::io(&format!("commit:{n}")).is_err());
    let status = if injected {
        NEEDS_RECONNECT.with(|c| c.set(true));
        tracing::error!("injected commit failure");
        drop(tx);
        CommitStatus::Uncertain
    } else {
        match tx.commit() {
            Ok(()) => {
                for n in &names {
                    crate::failpoint::hit(&format!("after_commit:{n}"));
                }
                CommitStatus::Committed
            }
            Err(e) => {
                let definite = matches!(
                    e.sqlite_error_code(),
                    Some(rusqlite::ErrorCode::DatabaseBusy)
                        | Some(rusqlite::ErrorCode::DatabaseLocked)
                );
                if definite {
                    CommitStatus::NotCommitted
                } else {
                    NEEDS_RECONNECT.with(|c| c.set(true));
                    tracing::error!(error = %e, "metadata group commit failed with unknown outcome");
                    CommitStatus::Uncertain
                }
            }
        }
    };
    for fin in finishers {
        fin(status);
    }
}

fn spawn_worker(
    name: &str,
    conn: Connection,
    mut rx: mpsc::Receiver<Msg>,
    path: PathBuf,
    role: Role,
    busy_timeout_ms: u64,
    stats: Arc<DbStats>,
) -> Result<JoinHandle<()>> {
    let handle = std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            let mut conn = Some(conn);
            let depth = match role {
                Role::Writer => &stats.write_queue_depth,
                Role::Reader => &stats.read_queue_depth,
            };
            let mut pending: std::collections::VecDeque<Msg> = std::collections::VecDeque::new();
            loop {
                let msg = match pending.pop_front() {
                    Some(m) => m,
                    None => match rx.blocking_recv() {
                        Some(m) => {
                            if !matches!(m, Msg::Stop) {
                                depth.fetch_sub(1, Ordering::Relaxed);
                            }
                            m
                        }
                        None => break,
                    },
                };
                if matches!(msg, Msg::Stop) {
                    break;
                }
                if conn.is_none() {
                    match open_connection(&path, role, busy_timeout_ms) {
                        Ok(c) => conn = Some(c),
                        Err(e) => {
                            tracing::error!(error = %e, "cannot reopen metadata connection");
                            // The job's sender observes a dropped oneshot.
                            drop(msg);
                            continue;
                        }
                    }
                }
                let c = conn.as_mut().expect("connection present");
                let started = Instant::now();
                match msg {
                    Msg::Job(job) => job(c),
                    Msg::Tx(name, body) => {
                        // Gather already-queued transactions for one commit.
                        let mut batch = vec![(name, body)];
                        while batch.len() < MAX_GROUP_COMMIT {
                            match rx.try_recv() {
                                Ok(Msg::Tx(n, b)) => {
                                    depth.fetch_sub(1, Ordering::Relaxed);
                                    batch.push((n, b));
                                }
                                Ok(other) => {
                                    if !matches!(other, Msg::Stop) {
                                        depth.fetch_sub(1, Ordering::Relaxed);
                                    }
                                    pending.push_back(other);
                                    break;
                                }
                                Err(_) => break,
                            }
                        }
                        run_group(c, batch);
                    }
                    Msg::Stop => unreachable!(),
                }
                if role == Role::Writer {
                    stats
                        .write_micros_total
                        .fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
                }
                if !c.is_autocommit() {
                    // A job left a transaction open: roll it back.
                    let _ = c.execute_batch("ROLLBACK");
                }
                if NEEDS_RECONNECT.with(|f| f.replace(false)) {
                    tracing::warn!("recycling metadata connection after an uncertain commit");
                    conn = None;
                }
            }
            if role == Role::Writer
                && let Some(c) = conn.as_ref()
            {
                let _ = c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
            }
        })
        .map_err(Error::Io)?;
    Ok(handle)
}

pub fn now_ms() -> i64 {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_sqlite_is_patched() {
        check_sqlite_runtime().unwrap();
        assert!(sqlite_source_id().len() > 20);
    }

    #[test]
    fn durability_settings_apply_to_every_connection() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.sqlite3");
        let conn = create_database(&path).unwrap();
        migrations::apply(&conn).unwrap();
        drop(conn);
        for role in [Role::Writer, Role::Reader] {
            let c = open_connection(&path, role, 1000).unwrap();
            let s = read_settings(&c).unwrap();
            assert_eq!(s.journal_mode, "wal");
            assert_eq!(s.synchronous, 2);
            assert_eq!(s.foreign_keys, 1);
        }
        let r = open_connection(&path, Role::Reader, 1000).unwrap();
        assert!(
            r.execute("INSERT INTO store_meta(key, value) VALUES('x', x'00')", [])
                .is_err()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bounded_queue_rejects_when_full() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.sqlite3");
        let conn = create_database(&path).unwrap();
        migrations::apply(&conn).unwrap();
        drop(conn);
        let cfg = DatabaseConfig {
            reader_connections: 1,
            writer_queue_capacity: 1,
            reader_queue_capacity: 1,
            busy_timeout_ms: 1000,
            queue_wait_ms: 50,
        };
        let db = Db::start(&path, &cfg).unwrap();
        let (block_tx, block_rx) = std::sync::mpsc::channel::<()>();
        let block_rx = Arc::new(Mutex::new(block_rx));
        // Occupy the worker, then fill the single queue slot.
        let b = block_rx.clone();
        let first = tokio::spawn({
            let db = db.clone();
            async move {
                db.write(move |_| {
                    let _ = b.lock().unwrap().recv();
                    Ok(())
                })
                .await
            }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let second = tokio::spawn({
            let db = db.clone();
            async move { db.write(|_| Ok(())).await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        let third = db.write(|_| Ok(())).await;
        assert!(matches!(third, Err(Error::Overloaded(_))), "{third:?}");
        block_tx.send(()).unwrap();
        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();
        db.shutdown();
    }
}
