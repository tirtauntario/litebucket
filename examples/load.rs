//! Small-object metadata-contention load test (spec §19.4).
//!
//! Starts an in-process server on a temporary store (production durability
//! settings) under `$TMPDIR` and drives concurrent 1 KiB PUT/GET/DELETE with
//! SigV4 header auth. Prints JSON.
//!
//!   cargo run --release --example load -- [ops] [concurrency]

#[path = "../tests/common/mod.rs"]
mod common;

use std::time::Instant;

use common::{Payload, TestServer};

fn pct(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let i = ((sorted.len() as f64 * p).ceil() as usize).clamp(1, sorted.len()) - 1;
    sorted[i]
}

async fn phase(c: &common::Client, op: &'static str, ops: usize, conc: usize) -> serde_json::Value {
    let started = Instant::now();
    let mut tasks = Vec::new();
    let per = ops / conc;
    for w in 0..conc {
        let c = c.clone();
        tasks.push(tokio::spawn(async move {
            let mut lat = Vec::with_capacity(per);
            for i in 0..per {
                let key = format!("/bench/k{w:03}-{i:06}");
                let t = Instant::now();
                let r = match op {
                    "put" => {
                        c.send("PUT", &key, "", &[], Payload::Signed(vec![7u8; 1024]))
                            .await
                    }
                    "get" => c.get(&key, "").await,
                    _ => c.delete(&key).await,
                };
                assert!(r.status < 300, "{op} {}: {}", r.status, r.text());
                lat.push(t.elapsed().as_secs_f64() * 1000.0);
            }
            lat
        }));
    }
    let mut lat = Vec::new();
    for t in tasks {
        lat.extend(t.await.unwrap());
    }
    let wall = started.elapsed().as_secs_f64();
    lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
    serde_json::json!({
        "ops": lat.len(),
        "ops_per_s": (lat.len() as f64 / wall).round(),
        "p50_ms": (pct(&lat, 0.50) * 100.0).round() / 100.0,
        "p99_ms": (pct(&lat, 0.99) * 100.0).round() / 100.0,
    })
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let ops: usize = args.get(1).and_then(|v| v.parse().ok()).unwrap_or(4000);
    let conc: usize = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(16);
    let s = TestServer::start().await;
    let c = s.admin();
    c.create_bucket("bench").await;
    let put = phase(&c, "put", ops, conc).await;
    let get = phase(&c, "get", ops, conc).await;
    let del = phase(&c, "delete", ops, conc).await;
    let out = serde_json::json!({
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "data_dir": s.data_dir(),
        "concurrency": conc,
        "put_1kib": put,
        "get_1kib": get,
        "delete": del,
        "db_writer_jobs": s.store().db.stats().write_jobs.load(std::sync::atomic::Ordering::Relaxed),
    });
    println!("{}", serde_json::to_string_pretty(&out).unwrap());
}
