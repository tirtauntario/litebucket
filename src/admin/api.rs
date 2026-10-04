//! JSON admin API on a local Unix socket.
//!
//! Only peers whose effective uid is the server's own or root are served
//! (checked with the socket's peer credentials); the socket file is also
//! mode 0600. Every mutation runs as one metadata transaction together with
//! its audit record, then the in-memory credential snapshot is rebuilt before
//! the response is sent, so a new or revoked key takes effect on the very
//! next S3 request.

use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path as UrlPath, Query, State};
use axum::routing::{get, post, put};
use axum::{Extension, Json};
use http::StatusCode;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use hyper_util::service::TowerToHyperService;
use rusqlite::Connection;
use serde::de::DeserializeOwned;
use tokio::net::UnixListener;
use tokio::sync::watch;

use super::{
    CorsRequest, CreateBucketRequest, CreateKeyRequest, Ctx, GrantJson, QuotaRequest,
    RemoveGrantRequest, RotateKeyRequest, UpdateKeyRequest,
};
use crate::error::{Error, Result};
use crate::metadata::{now_ms, queries};
use crate::store::Store;

/// Who is calling, from the socket's peer credentials (for the audit log).
#[derive(Debug, Clone)]
pub struct Actor(pub String);

/// Maximum admin request body.
const MAX_BODY: usize = 1024 * 1024;

pub struct ApiError(Error);

impl From<Error> for ApiError {
    fn from(e: Error) -> Self {
        Self(e)
    }
}

impl axum::response::IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let (status, code) = match &self.0 {
            Error::Config(_) => (StatusCode::BAD_REQUEST, "invalid_request"),
            Error::NotFound(_) => (StatusCode::NOT_FOUND, "not_found"),
            Error::Conflict(_) => (StatusCode::CONFLICT, "conflict"),
            Error::Overloaded(_) => (StatusCode::SERVICE_UNAVAILABLE, "overloaded"),
            Error::CommitUncertain => (StatusCode::INTERNAL_SERVER_ERROR, "commit_uncertain"),
            _ => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        };
        let message = match &self.0 {
            Error::Config(m) | Error::NotFound(m) | Error::Conflict(m) => m.clone(),
            other => other.to_string(),
        };
        json_response(
            status,
            &serde_json::json!({ "error": { "code": code, "message": message } }),
        )
    }
}

type ApiResult = std::result::Result<axum::response::Response, ApiError>;

fn json_response<T: serde::Serialize>(status: StatusCode, v: &T) -> axum::response::Response {
    let mut r = axum::response::IntoResponse::into_response(Json(v));
    *r.status_mut() = status;
    r
}

fn ok<T: serde::Serialize>(v: T) -> ApiResult {
    Ok(json_response(StatusCode::OK, &v))
}

fn parse<T: DeserializeOwned>(body: &Bytes) -> Result<T> {
    let body: &[u8] = if body.is_empty() { b"{}" } else { body };
    serde_json::from_slice(body).map_err(|e| Error::config(format!("invalid JSON body: {e}")))
}

/// Run an admin mutation: one audited metadata transaction, then refresh the
/// credential snapshot before answering.
async fn mutate<T, F>(store: &Arc<Store>, actor: &Actor, f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&Connection, &Ctx<'_>) -> Result<T> + Send + 'static,
{
    let _guard = store.admin_lock.lock().await;
    let st = store.clone();
    let who = actor.0.clone();
    let out = store
        .db
        .write_tx("admin", move |tx| {
            let cx = Ctx {
                codec: &st.secrets,
                store_id: &st.meta.store_id,
                actor: &who,
                now_ms: now_ms(),
                max_buckets: st.config.limits.max_buckets,
            };
            f(tx, &cx)
        })
        .await?;
    tracing::info!(event = "admin_change", actor = %actor.0, "admin change committed");
    if let Err(e) = store.refresh_credentials().await {
        tracing::error!(event = "credential_refresh_failed", error = %e, "admin change committed but the key snapshot was not refreshed");
        return Err(Error::other(format!(
            "the change was committed but is not yet active ({e}); restart the server to apply it"
        )));
    }
    Ok(out)
}

async fn read<T, F>(store: &Arc<Store>, f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&Connection) -> Result<T> + Send + 'static,
{
    store.db.read(move |c| f(c)).await
}

pub fn router(store: Arc<Store>) -> Router {
    Router::new()
        .route("/v1/status", get(status))
        .route("/v1/keys", get(list_keys).post(create_key))
        .route(
            "/v1/keys/{id}",
            get(get_key).patch(update_key).delete(delete_key),
        )
        .route("/v1/keys/{id}/rotate", post(rotate_key))
        .route("/v1/keys/{id}/grants", post(put_grant))
        .route("/v1/keys/{id}/grants/remove", post(remove_grant))
        .route(
            "/v1/keys/{id}/global-grants/{name}",
            put(put_global_grant).delete(remove_global_grant),
        )
        .route("/v1/buckets", get(list_buckets).post(create_bucket))
        .route("/v1/buckets/{name}", get(get_bucket).delete(delete_bucket))
        .route("/v1/buckets/{name}/quota", put(set_quota))
        .route("/v1/buckets/{name}/cors", put(put_cors).delete(delete_cors))
        .route("/v1/audit", get(audit))
        .fallback(not_found)
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY))
        .with_state(store)
}

async fn not_found() -> ApiResult {
    Err(Error::NotFound("no such admin endpoint".into()).into())
}

async fn status(State(store): State<Arc<Store>>) -> ApiResult {
    let st = store.clone();
    let set = store.credentials.snapshot();
    ok(read(&store, move |c| {
        super::status(c, &st.meta, &st.secrets, &set)
    })
    .await?)
}

async fn list_keys(State(store): State<Arc<Store>>) -> ApiResult {
    ok(read(&store, |c| super::list_keys(c, now_ms())).await?)
}

async fn get_key(State(store): State<Arc<Store>>, UrlPath(id): UrlPath<String>) -> ApiResult {
    ok(read(&store, move |c| super::get_key(c, &id, now_ms())).await?)
}

async fn create_key(
    State(store): State<Arc<Store>>,
    Extension(actor): Extension<Actor>,
    body: Bytes,
) -> ApiResult {
    let req: CreateKeyRequest = parse(&body)?;
    let issued = mutate(&store, &actor, move |c, cx| super::create_key(c, cx, &req)).await?;
    Ok(json_response(StatusCode::CREATED, &issued))
}

async fn update_key(
    State(store): State<Arc<Store>>,
    Extension(actor): Extension<Actor>,
    UrlPath(id): UrlPath<String>,
    body: Bytes,
) -> ApiResult {
    let req: UpdateKeyRequest = parse(&body)?;
    ok(mutate(&store, &actor, move |c, cx| {
        super::update_key(c, cx, &id, &req)
    })
    .await?)
}

async fn delete_key(
    State(store): State<Arc<Store>>,
    Extension(actor): Extension<Actor>,
    UrlPath(id): UrlPath<String>,
) -> ApiResult {
    mutate(&store, &actor, move |c, cx| super::delete_key(c, cx, &id)).await?;
    ok(serde_json::json!({ "deleted": true }))
}

async fn rotate_key(
    State(store): State<Arc<Store>>,
    Extension(actor): Extension<Actor>,
    UrlPath(id): UrlPath<String>,
    body: Bytes,
) -> ApiResult {
    let req: RotateKeyRequest = parse(&body)?;
    ok(mutate(&store, &actor, move |c, cx| {
        super::rotate_key(c, cx, &id, &req)
    })
    .await?)
}

async fn put_grant(
    State(store): State<Arc<Store>>,
    Extension(actor): Extension<Actor>,
    UrlPath(id): UrlPath<String>,
    body: Bytes,
) -> ApiResult {
    let g: GrantJson = parse(&body)?;
    ok(mutate(&store, &actor, move |c, cx| {
        super::put_grant(c, cx, &id, &g)
    })
    .await?)
}

async fn remove_grant(
    State(store): State<Arc<Store>>,
    Extension(actor): Extension<Actor>,
    UrlPath(id): UrlPath<String>,
    body: Bytes,
) -> ApiResult {
    let req: RemoveGrantRequest = parse(&body)?;
    ok(mutate(&store, &actor, move |c, cx| {
        super::remove_grant(c, cx, &id, &req)
    })
    .await?)
}

async fn put_global_grant(
    State(store): State<Arc<Store>>,
    Extension(actor): Extension<Actor>,
    UrlPath((id, name)): UrlPath<(String, String)>,
) -> ApiResult {
    ok(mutate(&store, &actor, move |c, cx| {
        super::put_global_grant(c, cx, &id, &name)
    })
    .await?)
}

async fn remove_global_grant(
    State(store): State<Arc<Store>>,
    Extension(actor): Extension<Actor>,
    UrlPath((id, name)): UrlPath<(String, String)>,
) -> ApiResult {
    ok(mutate(&store, &actor, move |c, cx| {
        super::remove_global_grant(c, cx, &id, &name)
    })
    .await?)
}

async fn list_buckets(State(store): State<Arc<Store>>) -> ApiResult {
    ok(read(&store, super::list_buckets).await?)
}

async fn get_bucket(State(store): State<Arc<Store>>, UrlPath(name): UrlPath<String>) -> ApiResult {
    ok(read(&store, move |c| super::get_bucket(c, &name)).await?)
}

async fn create_bucket(
    State(store): State<Arc<Store>>,
    Extension(actor): Extension<Actor>,
    body: Bytes,
) -> ApiResult {
    let req: CreateBucketRequest = parse(&body)?;
    let b = mutate(&store, &actor, move |c, cx| {
        super::create_bucket(c, cx, &req)
    })
    .await?;
    Ok(json_response(StatusCode::CREATED, &b))
}

async fn delete_bucket(
    State(store): State<Arc<Store>>,
    Extension(actor): Extension<Actor>,
    UrlPath(name): UrlPath<String>,
) -> ApiResult {
    mutate(&store, &actor, move |c, cx| {
        super::delete_bucket(c, cx, &name)
    })
    .await?;
    ok(serde_json::json!({ "deleted": true }))
}

async fn set_quota(
    State(store): State<Arc<Store>>,
    Extension(actor): Extension<Actor>,
    UrlPath(name): UrlPath<String>,
    body: Bytes,
) -> ApiResult {
    let req: QuotaRequest = parse(&body)?;
    ok(mutate(&store, &actor, move |c, cx| {
        super::set_quota(c, cx, &name, req.quota_bytes)
    })
    .await?)
}

async fn put_cors(
    State(store): State<Arc<Store>>,
    Extension(actor): Extension<Actor>,
    UrlPath(name): UrlPath<String>,
    body: Bytes,
) -> ApiResult {
    let req: CorsRequest = parse(&body)?;
    ok(mutate(&store, &actor, move |c, cx| {
        super::set_cors(c, cx, &name, Some(&req.rules))
    })
    .await?)
}

async fn delete_cors(
    State(store): State<Arc<Store>>,
    Extension(actor): Extension<Actor>,
    UrlPath(name): UrlPath<String>,
) -> ApiResult {
    ok(mutate(&store, &actor, move |c, cx| {
        super::set_cors(c, cx, &name, None)
    })
    .await?)
}

#[derive(serde::Deserialize)]
struct AuditQuery {
    limit: Option<usize>,
}

async fn audit(State(store): State<Arc<Store>>, Query(q): Query<AuditQuery>) -> ApiResult {
    let limit = q.limit.unwrap_or(100).clamp(1, 10_000);
    ok(read(&store, move |c| queries::list_audit(c, limit)).await?)
}

/// Bind the admin socket. A leftover socket from a crashed server is
/// replaced (the store lock guarantees no other server owns this store); a
/// socket another process still answers on, or any non-socket file, is an
/// error.
pub fn bind(path: &Path) -> Result<UnixListener> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty())
        && !parent.is_dir()
    {
        return Err(Error::config(format!(
            "admin socket directory {} does not exist",
            parent.display()
        )));
    }
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_socket() => {
            if std::os::unix::net::UnixStream::connect(path).is_ok() {
                return Err(Error::config(format!(
                    "admin socket {} is in use by another process",
                    path.display()
                )));
            }
            std::fs::remove_file(path)?;
        }
        Ok(_) => {
            return Err(Error::config(format!(
                "{} exists and is not a socket",
                path.display()
            )));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let listener = UnixListener::bind(path)
        .map_err(|e| Error::config(format!("cannot bind admin socket {}: {e}", path.display())))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Serve the admin API until `stop` flips, then remove the socket file.
pub async fn serve(
    listener: UnixListener,
    path: PathBuf,
    store: Arc<Store>,
    mut stop: watch::Receiver<bool>,
) {
    let router = router(store.clone());
    let server_uid = rustix::process::geteuid().as_raw();
    let header_timeout = Duration::from_secs(store.config.http.header_timeout_seconds);
    let graceful = GracefulShutdown::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let stream = match accepted {
                    Ok((s, _)) => s,
                    Err(e) => {
                        tracing::warn!(error = %e, "admin accept failed");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                };
                let uid = match stream.peer_cred() {
                    Ok(c) => c.uid(),
                    Err(e) => {
                        tracing::warn!(error = %e, "admin peer credentials unavailable; connection refused");
                        continue;
                    }
                };
                if uid != server_uid && uid != 0 {
                    tracing::warn!(event = "admin_peer_refused", uid, "admin connection from another user refused");
                    continue;
                }
                let svc = TowerToHyperService::new(
                    router.clone().layer(Extension(Actor(format!("uid:{uid}")))),
                );
                let watcher = graceful.watcher();
                tokio::spawn(async move {
                    let mut builder = hyper::server::conn::http1::Builder::new();
                    builder
                        .timer(TokioTimer::new())
                        .header_read_timeout(header_timeout)
                        .keep_alive(true);
                    let conn = builder.serve_connection(TokioIo::new(stream), svc);
                    let _ = watcher.watch(conn).await;
                });
            }
            _ = async { let _ = stop.wait_for(|v| *v).await; } => break,
        }
    }
    drop(listener);
    let _ = tokio::time::timeout(Duration::from_secs(5), graceful.shutdown()).await;
    let _ = std::fs::remove_file(&path);
}
