//! Supervised in-process maintenance: garbage collection, multipart expiry,
//! receipt expiry, WAL checkpoints, and periodic counter verification.
//!
//! Garbage collection only deletes paths derived from validated tracked IDs
//! whose rows are GARBAGE, unreferenced, eligible, and not owned by an active
//! operation. It never walks or recursively deletes directories.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::error::Result;
use crate::fsutil::{Area, sync_dir};
use crate::metadata::now_ms;
use crate::metadata::queries;
use crate::store::{Store, blocking};

pub struct Maintenance {
    cancel: CancellationToken,
    tasks: Vec<JoinHandle<()>>,
}

impl Maintenance {
    pub fn start(store: Arc<Store>) -> Self {
        let cancel = CancellationToken::new();
        let gc_every = Duration::from_secs(store.config.maintenance.garbage_interval_seconds);
        let mut tasks = Vec::new();
        tasks.push(spawn_loop(
            store.clone(),
            cancel.clone(),
            gc_every,
            "gc",
            |s| {
                Box::pin(async move {
                    // Drain the eligible backlog in bounded batches.
                    for _ in 0..100 {
                        if gc_once(&s).await? == 0 {
                            break;
                        }
                    }
                    Ok(())
                })
            },
        ));
        tasks.push(spawn_loop(
            store.clone(),
            cancel.clone(),
            Duration::from_secs(60),
            "expiry",
            |s| {
                Box::pin(async move {
                    expire_once(&s).await?;
                    forget_rotated_secrets(&s).await
                })
            },
        ));
        tasks.push(spawn_loop(
            store.clone(),
            cancel.clone(),
            Duration::from_secs(60),
            "checkpoint",
            |s| Box::pin(async move { checkpoint(&s).await }),
        ));
        tasks.push(spawn_loop(
            store.clone(),
            cancel.clone(),
            Duration::from_secs(30),
            "gauges",
            |s| Box::pin(async move { refresh_gauges(&s).await }),
        ));
        tasks.push(spawn_loop(
            store.clone(),
            cancel.clone(),
            Duration::from_secs(6 * 3600),
            "verify_counters",
            |s| Box::pin(async move { verify_counters(&s).await }),
        ));
        Self { cancel, tasks }
    }

    pub async fn stop(self) {
        self.cancel.cancel();
        for t in self.tasks {
            let _ = t.await;
        }
    }
}

type JobFn =
    fn(Arc<Store>) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>;

fn spawn_loop(
    store: Arc<Store>,
    cancel: CancellationToken,
    every: Duration,
    name: &'static str,
    job: JobFn,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // First run after one interval: startup recovery already handled
        // interrupted work, and this keeps startup free of background writes.
        let mut delay = every;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(delay) => {}
            }
            delay = every;
            if !store.is_ready() {
                continue;
            }
            if let Err(e) = job(store.clone()).await {
                tracing::warn!(task = name, error = %e, "maintenance task failed");
            }
        }
    })
}

/// Drop previous secrets whose rotation grace period ended, so they are no
/// longer stored. Authentication already ignores them after the deadline.
async fn forget_rotated_secrets(store: &Arc<Store>) -> Result<()> {
    // Same ordering guarantee as admin changes: commit and refresh together.
    let _guard = store.admin_lock.lock().await;
    let now = now_ms();
    let n = store
        .db
        .write_tx("", move |tx| {
            queries::clear_expired_previous_secrets(tx, now)
        })
        .await?;
    if n > 0 {
        tracing::info!(
            event = "rotated_secrets_forgotten",
            keys = n,
            "previous secrets past their grace period removed"
        );
        store.refresh_credentials().await?;
    }
    Ok(())
}

/// Reclaim one bounded batch of eligible garbage. Returns blobs reclaimed.
pub async fn gc_once(store: &Arc<Store>) -> Result<usize> {
    if store.halted_reason().is_some() {
        return Ok(0);
    }
    let limit = store.config.maintenance.garbage_batch_size;
    let now = now_ms();
    let batch = store
        .db
        .read(move |c| queries::garbage_batch(c, now, limit))
        .await?;
    let batch: Vec<_> = batch
        .into_iter()
        .filter(|(id, _, _)| !store.is_blob_active(id))
        .collect();
    if batch.is_empty() {
        return Ok(0);
    }
    let data = store.data.clone();
    let ids: Vec<_> = batch.iter().map(|(id, area, _)| (*id, *area)).collect();
    crate::failpoint::hit("gc_before_unlink");
    let removed = blocking(move || {
        let mut dirs = HashMap::new();
        let mut ok = Vec::new();
        for (id, area) in ids {
            let mut failed = false;
            for a in [area.fs_area(), Area::Staging] {
                match data.remove(a, &id) {
                    Ok(Some(dir)) => {
                        let hex = id.to_hex();
                        dirs.insert((a.dir_name(), hex[0..4].to_string()), dir);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(storage_id = %id, error = %e, "could not remove garbage file");
                        failed = true;
                    }
                }
            }
            if !failed {
                ok.push(id);
            }
        }
        // Make the unlinks durable before forgetting the rows.
        for dir in dirs.values() {
            sync_dir(dir)?;
        }
        Ok(ok)
    })
    .await?;
    crate::failpoint::hit("gc_after_unlink");
    let n = removed.len();
    let rows = removed.clone();
    let deleted = store
        .db
        .write_tx("", move |tx| {
            let mut n = 0;
            for id in &rows {
                if queries::delete_garbage_row(tx, id)? {
                    n += 1;
                }
            }
            Ok(n)
        })
        .await?;
    let bytes: u64 = batch
        .iter()
        .filter(|(id, _, _)| removed.contains(id))
        .map(|(_, _, s)| *s)
        .sum();
    store
        .metrics
        .gc_deleted_blobs
        .fetch_add(deleted as u64, Ordering::Relaxed);
    store
        .metrics
        .gc_deleted_bytes
        .fetch_add(bytes, Ordering::Relaxed);
    Ok(n)
}

/// Expire inactive OPEN uploads and old completion/abort receipts.
pub async fn expire_once(store: &Arc<Store>) -> Result<usize> {
    if store.halted_reason().is_some() {
        return Ok(0);
    }
    let cutoff = now_ms() - (store.config.multipart.inactive_expiration_seconds as i64) * 1000;
    let ids = store
        .db
        .read(move |c| queries::expired_open_uploads(c, cutoff, 100))
        .await?;
    let mut expired = 0;
    for id in ids {
        if store.is_upload_active(&id) {
            continue;
        }
        let _g = store.upload_locks.lock(id.clone()).await;
        if store.is_upload_active(&id) {
            continue;
        }
        let (now, after) = (now_ms(), store.garbage_after());
        let ttl = store.config.multipart.receipt_retention_seconds as i64 * 1000;
        let idc = id.clone();
        let released = store
            .db
            .write_tx("", move |tx| {
                    // Recheck inactivity under the guard.
                    let still: Option<i64> = tx
                        .query_row(
                            "SELECT 1 FROM multipart_uploads WHERE upload_id = ?1 AND state = 'open' AND last_activity_ms < ?2",
                            rusqlite::params![idc, cutoff],
                            |r| r.get(0),
                        )
                        .ok();
                    if still.is_none() {
                        return Ok(None);
                    }
                    queries::abort_upload(tx, &idc, now, now + ttl, after)
                })
            .await?;
        if let Some(bytes) = released {
            store.capacity.release_part_bytes(bytes);
            store
                .metrics
                .multipart_expired
                .fetch_add(1, Ordering::Relaxed);
            expired += 1;
            tracing::info!(
                event = "multipart_expired",
                "expired an inactive multipart upload"
            );
        }
    }
    let now = now_ms();
    store
        .db
        .write_tx("", move |tx| {
            queries::delete_expired_receipts(tx, now, 1000)
        })
        .await?;
    Ok(expired)
}

/// Bounded PASSIVE checkpoint through the writer connection.
pub async fn checkpoint(store: &Arc<Store>) -> Result<()> {
    store
        .db
        .write(|c| {
            c.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()))?;
            Ok(())
        })
        .await?;
    store.metrics.checkpoints.fetch_add(1, Ordering::Relaxed);
    let wal = store
        .data
        .root()
        .join(format!("{}-wal", crate::fsutil::DB_FILE));
    let size = std::fs::metadata(wal).map(|m| m.len()).unwrap_or(0);
    store.metrics.wal_bytes.store(size, Ordering::Relaxed);
    Ok(())
}

pub async fn refresh_gauges(store: &Arc<Store>) -> Result<()> {
    let (blobs, bytes, uploads, objects, logical) = store
        .db
        .read(|c| {
            let (b, y) = queries::garbage_backlog(c)?;
            let (o, l): (i64, i64) = c.query_row(
                "SELECT coalesce(sum(object_count), 0), coalesce(sum(logical_bytes), 0) FROM buckets",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            Ok((b, y, queries::active_upload_count(c)?, o, l))
        })
        .await?;
    store.set_maintenance_gauges(blobs as u64, bytes as u64, uploads as u64);
    store.set_logical_totals(objects as u64, logical as u64);
    Ok(())
}

/// Verify one bucket's counters per run (bounded work).
pub async fn verify_counters(store: &Arc<Store>) -> Result<()> {
    let buckets = store
        .db
        .read(|c| queries::list_buckets(c, "", "", 100_000))
        .await?;
    if buckets.is_empty() {
        return Ok(());
    }
    let idx = (now_ms() / 1000 / 21_600) as usize % buckets.len();
    let b = buckets[idx].clone();
    let id = b.id;
    let (count, bytes) = store
        .db
        .read(move |c| queries::bucket_actual_usage(c, &id))
        .await?;
    if count != b.object_count || bytes != b.logical_bytes {
        store.integrity_fault(&format!(
            "bucket {} counters ({}, {}) differ from actual usage ({count}, {bytes})",
            b.name, b.object_count, b.logical_bytes
        ));
    }
    Ok(())
}
