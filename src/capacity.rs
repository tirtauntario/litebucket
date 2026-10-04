//! Admission control: transfer permits and disk/temporary-space reservations.
//!
//! Every write reserves capacity before writing. Reservations are in-memory and
//! released on drop; durable state remains in SQLite.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::config::LimitsConfig;
use crate::error::{Error, Result};
use crate::fsutil::{DataDir, FsStats};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermitKind {
    Upload,
    Download,
    Copy,
    Assembly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapacityError {
    /// Free disk space or inodes would fall below the configured reserve.
    DiskReserve,
    /// Temporary bytes (committed parts + active staging) would exceed the cap.
    TemporaryLimit,
}

#[derive(Default, Debug)]
pub struct CapacityStats {
    pub rejected_admissions: AtomicU64,
    pub rejected_capacity: AtomicU64,
}

pub struct Capacity {
    uploads: Arc<Semaphore>,
    downloads: Arc<Semaphore>,
    copies: Arc<Semaphore>,
    assemblies: Arc<Semaphore>,
    limits: LimitsConfig,
    admission_timeout: Duration,
    /// Bytes reserved by in-flight writes (staging, copies, assembly output).
    reserved: Arc<AtomicU64>,
    /// Bytes held by committed multipart parts.
    part_bytes: AtomicU64,
    data: Arc<DataDir>,
    pub stats: CapacityStats,
}

impl std::fmt::Debug for Capacity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Capacity")
            .field("reserved", &self.reserved.load(Ordering::Relaxed))
            .field("part_bytes", &self.part_bytes.load(Ordering::Relaxed))
            .finish()
    }
}

impl Capacity {
    pub fn new(limits: &LimitsConfig, data: Arc<DataDir>, committed_part_bytes: u64) -> Self {
        Self {
            uploads: Arc::new(Semaphore::new(limits.active_uploads)),
            downloads: Arc::new(Semaphore::new(limits.active_downloads)),
            copies: Arc::new(Semaphore::new(limits.active_copies)),
            assemblies: Arc::new(Semaphore::new(limits.active_multipart_assemblies)),
            limits: limits.clone(),
            admission_timeout: Duration::from_millis(limits.admission_timeout_ms),
            reserved: Arc::new(AtomicU64::new(0)),
            part_bytes: AtomicU64::new(committed_part_bytes),
            data,
            stats: CapacityStats::default(),
        }
    }

    fn semaphore(&self, kind: PermitKind) -> &Arc<Semaphore> {
        match kind {
            PermitKind::Upload => &self.uploads,
            PermitKind::Download => &self.downloads,
            PermitKind::Copy => &self.copies,
            PermitKind::Assembly => &self.assemblies,
        }
    }

    /// Wait (bounded) for a transfer permit; timeout is a retryable overload.
    pub async fn acquire(&self, kind: PermitKind) -> Result<OwnedSemaphorePermit> {
        let sem = self.semaphore(kind).clone();
        match tokio::time::timeout(self.admission_timeout, sem.acquire_owned()).await {
            Ok(Ok(p)) => Ok(p),
            Ok(Err(_)) => Err(Error::Overloaded("service is shutting down")),
            Err(_) => {
                self.stats.rejected_admissions.fetch_add(1, Ordering::Relaxed);
                Err(Error::Overloaded(match kind {
                    PermitKind::Upload => "too many concurrent uploads",
                    PermitKind::Download => "too many concurrent downloads",
                    PermitKind::Copy => "too many concurrent copies",
                    PermitKind::Assembly => "too many concurrent multipart completions",
                }))
            }
        }
    }

    pub fn in_use(&self, kind: PermitKind) -> usize {
        let total = match kind {
            PermitKind::Upload => self.limits.active_uploads,
            PermitKind::Download => self.limits.active_downloads,
            PermitKind::Copy => self.limits.active_copies,
            PermitKind::Assembly => self.limits.active_multipart_assemblies,
        };
        total - self.semaphore(kind).available_permits()
    }

    fn floor(&self, st: &FsStats) -> u64 {
        let pct = st.total_bytes / 100 * self.limits.min_disk_free_percent;
        self.limits.min_disk_free_bytes.max(pct)
    }

    /// Check that `extra` more bytes can be written while keeping the reserve.
    fn check(&self, extra: u64) -> std::result::Result<(), CapacityError> {
        let reserved = self.reserved.load(Ordering::Acquire);
        let temp = self.part_bytes.load(Ordering::Acquire).saturating_add(reserved);
        if temp.saturating_add(extra) > self.limits.max_temporary_bytes {
            return Err(CapacityError::TemporaryLimit);
        }
        let st = self.data.fs_stats().map_err(|_| CapacityError::DiskReserve)?;
        let needed = reserved.saturating_add(extra).saturating_add(self.floor(&st));
        if st.avail_bytes < needed {
            return Err(CapacityError::DiskReserve);
        }
        if st.total_inodes > 0 && st.avail_inodes < self.limits.min_free_inodes.saturating_add(16) {
            return Err(CapacityError::DiskReserve);
        }
        Ok(())
    }

    /// Reserve `bytes` for a write of known (or initial) size.
    pub fn reserve(&self, bytes: u64) -> std::result::Result<Reservation, CapacityError> {
        if let Err(e) = self.check(bytes) {
            self.stats.rejected_capacity.fetch_add(1, Ordering::Relaxed);
            return Err(e);
        }
        self.reserved.fetch_add(bytes, Ordering::AcqRel);
        Ok(Reservation {
            bytes,
            counter: self.reserved.clone(),
        })
    }

    /// Grow a reservation (unknown-length streams) to at least `total` bytes.
    pub fn grow(&self, r: &mut Reservation, total: u64) -> std::result::Result<(), CapacityError> {
        if total <= r.bytes {
            return Ok(());
        }
        // Grow in 8 MiB steps to bound statvfs calls.
        let step = 8 * 1024 * 1024;
        let target = total.div_ceil(step) * step;
        let extra = target - r.bytes;
        if let Err(e) = self.check(extra) {
            self.stats.rejected_capacity.fetch_add(1, Ordering::Relaxed);
            return Err(e);
        }
        self.reserved.fetch_add(extra, Ordering::AcqRel);
        r.bytes = target;
        Ok(())
    }

    pub fn add_part_bytes(&self, n: u64) {
        self.part_bytes.fetch_add(n, Ordering::AcqRel);
    }

    pub fn release_part_bytes(&self, n: u64) {
        let _ = self
            .part_bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| Some(v.saturating_sub(n)));
    }

    pub fn part_bytes(&self) -> u64 {
        self.part_bytes.load(Ordering::Relaxed)
    }

    pub fn reserved_bytes(&self) -> u64 {
        self.reserved.load(Ordering::Relaxed)
    }

    /// Whether new writes would currently be refused for lack of space.
    pub fn under_pressure(&self) -> bool {
        self.check(0).is_err()
    }

    pub fn fs_stats(&self) -> Option<FsStats> {
        self.data.fs_stats().ok()
    }
}

/// Reserved bytes, released when dropped (after commit, failure, or cancel).
#[derive(Debug)]
pub struct Reservation {
    bytes: u64,
    counter: Arc<AtomicU64>,
}

impl Reservation {
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.counter.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(limits: LimitsConfig) -> (tempfile::TempDir, Capacity) {
        let tmp = tempfile::tempdir().unwrap();
        let dd = Arc::new(DataDir::create(&tmp.path().join("d")).unwrap());
        (tmp, Capacity::new(&limits, dd, 0))
    }

    #[test]
    fn concurrent_reservations_cannot_over_admit_temporary_space() {
        let limits = LimitsConfig {
            max_temporary_bytes: 100,
            min_disk_free_bytes: 0,
            min_disk_free_percent: 0,
            min_free_inodes: 0,
            ..Default::default()
        };
        let (_t, c) = cap(limits);
        let a = c.reserve(60).unwrap();
        assert_eq!(c.reserve(60).unwrap_err(), CapacityError::TemporaryLimit);
        drop(a);
        let _b = c.reserve(60).unwrap();
        c.add_part_bytes(40);
        assert_eq!(c.reserve(1).unwrap_err(), CapacityError::TemporaryLimit);
        c.release_part_bytes(40);
        assert!(c.reserve(40).is_ok());
    }

    #[test]
    fn disk_reserve_is_enforced() {
        let limits = LimitsConfig {
            min_disk_free_bytes: u64::MAX / 4,
            ..Default::default()
        };
        let (_t, c) = cap(limits);
        assert_eq!(c.reserve(1).unwrap_err(), CapacityError::DiskReserve);
        assert!(c.under_pressure());
    }

    #[test]
    fn reservations_release_on_drop() {
        let limits = LimitsConfig {
            min_disk_free_bytes: 0,
            min_disk_free_percent: 0,
            min_free_inodes: 0,
            ..Default::default()
        };
        let (_t, c) = cap(limits);
        let mut r = c.reserve(10).unwrap();
        c.grow(&mut r, 9 * 1024 * 1024).unwrap();
        assert_eq!(c.reserved_bytes(), 16 * 1024 * 1024);
        drop(r);
        assert_eq!(c.reserved_bytes(), 0);
    }

    #[tokio::test]
    async fn admission_times_out_with_overload() {
        let limits = LimitsConfig {
            active_uploads: 1,
            admission_timeout_ms: 20,
            ..Default::default()
        };
        let (_t, c) = cap(limits);
        let _p = c.acquire(PermitKind::Upload).await.unwrap();
        assert!(matches!(c.acquire(PermitKind::Upload).await, Err(Error::Overloaded(_))));
        assert_eq!(c.in_use(PermitKind::Upload), 1);
    }
}
