//! Structured logging setup and bounded-cardinality metrics.
//!
//! Metric labels are drawn from fixed sets (operation names, status classes).
//! Object keys, upload IDs, request IDs, and credential IDs are never labels.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::config::LoggingConfig;

pub const OPERATIONS: &[&str] = &[
    "ListBuckets",
    "CreateBucket",
    "HeadBucket",
    "DeleteBucket",
    "GetBucketLocation",
    "PutObject",
    "GetObject",
    "HeadObject",
    "DeleteObject",
    "ListObjectsV2",
    "DeleteObjects",
    "CopyObject",
    "CreateMultipartUpload",
    "UploadPart",
    "CompleteMultipartUpload",
    "AbortMultipartUpload",
    "ListParts",
    "ListMultipartUploads",
    "PutBucketCors",
    "GetBucketCors",
    "DeleteBucketCors",
    "CorsPreflight",
    "Unknown",
];

const LATENCY_BUCKETS_MS: &[u64] = &[5, 10, 50, 100, 500, 1000, 5000, 30_000];
const STATUS_CLASSES: &[&str] = &["1xx", "2xx", "3xx", "4xx", "5xx"];

pub fn op_index(name: &str) -> usize {
    OPERATIONS
        .iter()
        .position(|o| *o == name)
        .unwrap_or(OPERATIONS.len() - 1)
}

#[derive(Default)]
struct OpMetrics {
    status: [AtomicU64; 5],
    latency_buckets: [AtomicU64; 8],
    latency_count: AtomicU64,
    latency_sum_us: AtomicU64,
}

/// Process-wide counters. Gauges are sampled from live state at render time.
pub struct Metrics {
    ops: Vec<OpMetrics>,
    pub bytes_received: AtomicU64,
    pub bytes_sent: AtomicU64,
    pub integrity_errors: AtomicU64,
    pub recovery_actions: AtomicU64,
    pub gc_deleted_blobs: AtomicU64,
    pub gc_deleted_bytes: AtomicU64,
    pub multipart_expired: AtomicU64,
    pub multipart_aborted: AtomicU64,
    pub checkpoints: AtomicU64,
    pub wal_bytes: AtomicU64,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            ops: (0..OPERATIONS.len())
                .map(|_| OpMetrics::default())
                .collect(),
            bytes_received: AtomicU64::new(0),
            bytes_sent: AtomicU64::new(0),
            integrity_errors: AtomicU64::new(0),
            recovery_actions: AtomicU64::new(0),
            gc_deleted_blobs: AtomicU64::new(0),
            gc_deleted_bytes: AtomicU64::new(0),
            multipart_expired: AtomicU64::new(0),
            multipart_aborted: AtomicU64::new(0),
            checkpoints: AtomicU64::new(0),
            wal_bytes: AtomicU64::new(0),
        }
    }
}

impl Metrics {
    pub fn observe(&self, op: &str, status: u16, elapsed: std::time::Duration) {
        let m = &self.ops[op_index(op)];
        let class = ((status / 100) as usize).clamp(1, 5) - 1;
        m.status[class].fetch_add(1, Ordering::Relaxed);
        let ms = elapsed.as_millis() as u64;
        for (i, b) in LATENCY_BUCKETS_MS.iter().enumerate() {
            if ms <= *b {
                m.latency_buckets[i].fetch_add(1, Ordering::Relaxed);
            }
        }
        m.latency_count.fetch_add(1, Ordering::Relaxed);
        m.latency_sum_us
            .fetch_add(elapsed.as_micros() as u64, Ordering::Relaxed);
    }

    /// Prometheus text exposition. `gauges` are (name, help, value) samples.
    pub fn render(&self, gauges: &[(&str, &str, f64)]) -> String {
        let mut out = String::new();
        out.push_str(
            "# HELP litebucket_requests_total S3 requests by operation and status class.\n",
        );
        out.push_str("# TYPE litebucket_requests_total counter\n");
        for (i, op) in OPERATIONS.iter().enumerate() {
            for (c, class) in STATUS_CLASSES.iter().enumerate() {
                let v = self.ops[i].status[c].load(Ordering::Relaxed);
                if v > 0 {
                    let _ = writeln!(
                        out,
                        "litebucket_requests_total{{operation=\"{op}\",status=\"{class}\"}} {v}"
                    );
                }
            }
        }
        out.push_str("# HELP litebucket_request_duration_seconds S3 request latency.\n");
        out.push_str("# TYPE litebucket_request_duration_seconds histogram\n");
        for (i, op) in OPERATIONS.iter().enumerate() {
            let m = &self.ops[i];
            let count = m.latency_count.load(Ordering::Relaxed);
            if count == 0 {
                continue;
            }
            for (b, le) in LATENCY_BUCKETS_MS.iter().enumerate() {
                let _ = writeln!(
                    out,
                    "litebucket_request_duration_seconds_bucket{{operation=\"{op}\",le=\"{}\"}} {}",
                    *le as f64 / 1000.0,
                    m.latency_buckets[b].load(Ordering::Relaxed)
                );
            }
            let _ = writeln!(
                out,
                "litebucket_request_duration_seconds_bucket{{operation=\"{op}\",le=\"+Inf\"}} {count}"
            );
            let _ = writeln!(
                out,
                "litebucket_request_duration_seconds_sum{{operation=\"{op}\"}} {}",
                m.latency_sum_us.load(Ordering::Relaxed) as f64 / 1e6
            );
            let _ = writeln!(
                out,
                "litebucket_request_duration_seconds_count{{operation=\"{op}\"}} {count}"
            );
        }
        let counters: [(&str, &str, &AtomicU64); 10] = [
            (
                "litebucket_bytes_received_total",
                "Decoded object bytes received.",
                &self.bytes_received,
            ),
            (
                "litebucket_bytes_sent_total",
                "Object bytes sent.",
                &self.bytes_sent,
            ),
            (
                "litebucket_integrity_errors_total",
                "Storage integrity faults detected.",
                &self.integrity_errors,
            ),
            (
                "litebucket_recovery_actions_total",
                "Recovery actions taken.",
                &self.recovery_actions,
            ),
            (
                "litebucket_gc_deleted_blobs_total",
                "Garbage files reclaimed.",
                &self.gc_deleted_blobs,
            ),
            (
                "litebucket_gc_deleted_bytes_total",
                "Garbage bytes reclaimed.",
                &self.gc_deleted_bytes,
            ),
            (
                "litebucket_multipart_expired_total",
                "Inactive multipart uploads expired.",
                &self.multipart_expired,
            ),
            (
                "litebucket_multipart_aborted_total",
                "Multipart uploads aborted by clients.",
                &self.multipart_aborted,
            ),
            (
                "litebucket_sqlite_checkpoints_total",
                "Passive WAL checkpoints run.",
                &self.checkpoints,
            ),
            (
                "litebucket_sqlite_wal_bytes",
                "WAL file size at last checkpoint.",
                &self.wal_bytes,
            ),
        ];
        for (name, help, v) in counters {
            let kind = if name.ends_with("_total") {
                "counter"
            } else {
                "gauge"
            };
            let _ = writeln!(
                out,
                "# HELP {name} {help}\n# TYPE {name} {kind}\n{name} {}",
                v.load(Ordering::Relaxed)
            );
        }
        for (name, help, v) in gauges {
            let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} gauge\n{name} {v}");
        }
        out
    }
}

/// Initialize the global tracing subscriber (idempotent for tests).
pub fn init_logging(cfg: &LoggingConfig) {
    let filter = tracing_subscriber::EnvFilter::try_new(&cfg.level)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false);
    let _ = if cfg.format == "json" {
        builder
            .json()
            .flatten_event(true)
            .with_current_span(false)
            .try_init()
    } else {
        builder.try_init()
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_has_bounded_labels() {
        let m = Metrics::default();
        m.observe("PutObject", 200, std::time::Duration::from_millis(3));
        m.observe("no-such-op", 500, std::time::Duration::from_millis(3));
        let text = m.render(&[("litebucket_active_uploads", "Active uploads.", 2.0)]);
        assert!(
            text.contains("litebucket_requests_total{operation=\"PutObject\",status=\"2xx\"} 1")
        );
        assert!(text.contains("litebucket_requests_total{operation=\"Unknown\",status=\"5xx\"} 1"));
        assert!(text.contains("litebucket_active_uploads 2"));
    }
}
