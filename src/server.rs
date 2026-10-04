//! Listeners: the S3 endpoint (HTTP/1.1, optional TLS) and the restricted
//! management endpoint (`/livez`, `/readyz`, `/metrics`), plus graceful
//! shutdown.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::routing::get;
use http::{Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::capacity::PermitKind;
use crate::error::{Error, Result};
use crate::store::Store;

pub struct Running {
    pub s3_addr: SocketAddr,
    pub management_addr: SocketAddr,
    stop: watch::Sender<bool>,
    s3_task: JoinHandle<()>,
    mgmt_task: JoinHandle<()>,
    maintenance: crate::maintenance::Maintenance,
    pub store: Arc<Store>,
}

fn load_tls(cfg: &crate::config::Config) -> Result<Option<tokio_rustls::TlsAcceptor>> {
    let (Some(cert), Some(key)) = (&cfg.http.tls_certificate_file, &cfg.http.tls_private_key_file) else {
        return Ok(None);
    };
    let certs = rustls_pemfile::certs(&mut std::io::BufReader::new(std::fs::File::open(cert)?))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| Error::config(format!("invalid TLS certificate file: {e}")))?;
    let key = rustls_pemfile::private_key(&mut std::io::BufReader::new(std::fs::File::open(key)?))
        .map_err(|e| Error::config(format!("invalid TLS key file: {e}")))?
        .ok_or_else(|| Error::config("TLS key file contains no private key"))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::config(format!("TLS configuration: {e}")))?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| Error::config(format!("TLS certificate/key mismatch: {e}")))?;
    let mut config = config;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Some(tokio_rustls::TlsAcceptor::from(Arc::new(config))))
}

pub fn s3_router(store: Arc<Store>) -> Router {
    Router::new().fallback(crate::s3::handle).with_state(store)
}

/// Bind listeners and start serving. The store must already be open and
/// recovered; readiness is set once both listeners are bound.
pub async fn start(store: Arc<Store>) -> Result<Running> {
    let cfg = store.config.clone();
    let tls = load_tls(&cfg)?;
    let allowed_peers: Option<Vec<IpAddr>> = cfg
        .http
        .trusted_proxy_mode
        .then(|| cfg.trusted_proxy_peers())
        .transpose()?;
    let (stop_tx, stop_rx) = watch::channel(false);

    let mgmt_listener = TcpListener::bind(cfg.management_listen()?).await?;
    let management_addr = mgmt_listener.local_addr()?;
    let mgmt = management_router(store.clone());
    let mut mgmt_stop = stop_rx.clone();
    let mgmt_task = tokio::spawn(async move {
        let _ = axum::serve(mgmt_listener, mgmt)
            .with_graceful_shutdown(async move {
                let _ = mgmt_stop.wait_for(|v| *v).await;
            })
            .await;
    });

    let listener = TcpListener::bind(cfg.http_listen()?).await?;
    let s3_addr = listener.local_addr()?;
    let router = s3_router(store.clone());
    let header_timeout = Duration::from_secs(cfg.http.header_timeout_seconds);
    let max_headers = cfg.http.max_header_count;
    let max_buf = cfg.http.max_header_bytes.max(8192);
    let grace = Duration::from_secs(cfg.maintenance.shutdown_grace_seconds);
    let mut s3_stop = stop_rx.clone();
    let s3_task = tokio::spawn(async move {
        let graceful = GracefulShutdown::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (stream, peer) = match accepted {
                        Ok(x) => x,
                        Err(e) => {
                            tracing::warn!(error = %e, "accept failed");
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            continue;
                        }
                    };
                    if let Some(peers) = &allowed_peers
                        && !peers.contains(&peer.ip())
                    {
                        tracing::warn!(event = "rejected_peer", "connection from a non-proxy peer refused");
                        continue;
                    }
                    let _ = stream.set_nodelay(true);
                    let svc = TowerToHyperService::new(router.clone());
                    let watcher = graceful.watcher();
                    let tls = tls.clone();
                    tokio::spawn(async move {
                        let mut builder = hyper::server::conn::http1::Builder::new();
                        builder
                            .timer(TokioTimer::new())
                            .header_read_timeout(header_timeout)
                            .max_headers(max_headers)
                            .max_buf_size(max_buf)
                            .keep_alive(true);
                        match tls {
                            Some(acceptor) => {
                                let Ok(Ok(s)) = tokio::time::timeout(header_timeout, acceptor.accept(stream)).await else {
                                    return;
                                };
                                let conn = builder.serve_connection(TokioIo::new(s), svc);
                                let _ = watcher.watch(conn).await;
                            }
                            None => {
                                let conn = builder.serve_connection(TokioIo::new(stream), svc);
                                let _ = watcher.watch(conn).await;
                            }
                        }
                    });
                }
                _ = async { let _ = s3_stop.wait_for(|v| *v).await; } => break,
            }
        }
        drop(listener);
        if tokio::time::timeout(grace, graceful.shutdown()).await.is_err() {
            tracing::warn!("connections still open after the shutdown grace period");
        }
    });
    let maintenance = crate::maintenance::Maintenance::start(store.clone());
    store.set_ready(true);
    crate::failpoint::arm();
    tracing::info!(
        event = "listening",
        s3 = %s3_addr,
        management = %management_addr,
        tls = cfg.tls_enabled(),
        "storlite is ready"
    );
    Ok(Running {
        s3_addr,
        management_addr,
        stop: stop_tx,
        s3_task,
        mgmt_task,
        maintenance,
        store,
    })
}

impl Running {
    /// Graceful shutdown: readiness off, stop accepting, drain requests and
    /// supervised commits within the grace period, stop workers, release the
    /// store lock last. Returns whether everything drained.
    pub async fn shutdown(self) -> bool {
        let store = self.store.clone();
        store.set_ready(false);
        let grace = Duration::from_secs(store.config.maintenance.shutdown_grace_seconds);
        let _ = self.stop.send(true);
        let s3_drained = tokio::time::timeout(grace + Duration::from_secs(1), self.s3_task).await.is_ok();
        let _ = self.mgmt_task.await;
        self.maintenance.stop().await;
        store.tracker.close();
        let tasks_drained = tokio::time::timeout(grace, store.tracker.wait()).await.is_ok();
        let db = store.db.clone();
        let _ = tokio::task::spawn_blocking(move || db.shutdown()).await;
        let drained = s3_drained && tasks_drained;
        tracing::info!(event = "shutdown", drained, "storlite stopped");
        drained
    }
}

fn management_router(store: Arc<Store>) -> Router {
    let metrics_enabled = store.config.management.metrics_enabled;
    let mut r = Router::new().route("/livez", get(livez)).route("/readyz", get(readyz));
    if metrics_enabled {
        r = r.route("/metrics", get(metrics));
    }
    r.with_state(store)
}

async fn livez() -> &'static str {
    "ok\n"
}

async fn readyz(State(store): State<Arc<Store>>) -> Response<Body> {
    let halted = store.halted_reason();
    let integrity = store.integrity_failed();
    let pressure = store.capacity.under_pressure();
    let ready = store.is_ready() && halted.is_none();
    let state = if !store.is_ready() {
        "starting_or_stopping"
    } else if halted.is_some() {
        "halted"
    } else if integrity {
        "integrity_failure"
    } else if pressure {
        "read_only_capacity_pressure"
    } else {
        "writable"
    };
    let body = serde_json::json!({
        "ready": ready,
        "state": state,
        "integrity_failure": integrity,
        "capacity_pressure": pressure,
        "mutations_halted": halted.is_some(),
    });
    let mut r = Response::new(Body::from(body.to_string()));
    *r.status_mut() = if ready && !integrity { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };
    r.headers_mut()
        .insert(http::header::CONTENT_TYPE, http::HeaderValue::from_static("application/json"));
    r
}

async fn metrics(State(store): State<Arc<Store>>) -> Response<Body> {
    let c = &store.capacity;
    let fs = c.fs_stats();
    let db = store.db.stats();
    let gauges = [
        ("storlite_active_uploads", "Uploads (including part reception) in progress.", c.in_use(PermitKind::Upload) as f64),
        ("storlite_active_downloads", "Downloads in progress.", c.in_use(PermitKind::Download) as f64),
        ("storlite_active_copies", "Server-side copies in progress.", c.in_use(PermitKind::Copy) as f64),
        ("storlite_active_assemblies", "Multipart assemblies in progress.", c.in_use(PermitKind::Assembly) as f64),
        ("storlite_rejected_admissions_total", "Requests rejected waiting for a transfer permit.", c.stats.rejected_admissions.load(Ordering::Relaxed) as f64),
        ("storlite_rejected_capacity_total", "Writes rejected for disk or temporary-space limits.", c.stats.rejected_capacity.load(Ordering::Relaxed) as f64),
        ("storlite_reserved_bytes", "Bytes reserved by in-flight writes.", c.reserved_bytes() as f64),
        ("storlite_multipart_part_bytes", "Bytes held by committed multipart parts.", c.part_bytes() as f64),
        ("storlite_fs_available_bytes", "Filesystem bytes available to the service.", fs.map(|f| f.avail_bytes as f64).unwrap_or(0.0)),
        ("storlite_fs_available_inodes", "Filesystem inodes available.", fs.map(|f| f.avail_inodes as f64).unwrap_or(0.0)),
        ("storlite_db_write_queue_depth", "Queued metadata write jobs.", db.write_queue_depth.load(Ordering::Relaxed) as f64),
        ("storlite_db_read_queue_depth", "Queued metadata read jobs.", db.read_queue_depth.load(Ordering::Relaxed) as f64),
        ("storlite_db_queue_rejections_total", "Metadata jobs rejected for a full queue.", db.queue_rejections.load(Ordering::Relaxed) as f64),
        ("storlite_db_write_jobs_total", "Metadata write jobs.", db.write_jobs.load(Ordering::Relaxed) as f64),
        ("storlite_db_write_seconds_total", "Time spent in metadata write jobs.", db.write_micros_total.load(Ordering::Relaxed) as f64 / 1e6),
        ("storlite_db_commit_uncertain_total", "Commits with an initially unknown outcome.", db.commit_uncertain.load(Ordering::Relaxed) as f64),
        ("storlite_garbage_backlog_blobs", "Tracked garbage awaiting deletion.", store.maintenance_gauges().0 as f64),
        ("storlite_garbage_backlog_bytes", "Tracked garbage bytes awaiting deletion.", store.maintenance_gauges().1 as f64),
        ("storlite_multipart_active_uploads", "Open or completing multipart uploads.", store.maintenance_gauges().2 as f64),
        ("storlite_ready", "1 when serving.", if store.is_ready() { 1.0 } else { 0.0 }),
    ];
    let text = store.metrics.render(&gauges);
    let mut r = Response::new(Body::from(text));
    r.headers_mut().insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("text/plain; version=0.0.4"),
    );
    r
}
