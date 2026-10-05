//! S3 protocol boundary: routing, authentication, capability validation,
//! request parsing, and XML/HTTP responses.

pub mod auth;
pub mod bucket;
pub mod capabilities;
pub mod cors;
pub mod error;
pub mod headers;
pub mod integrity;
pub mod listing;
pub mod multipart;
pub mod object;
pub mod payload;
pub mod request;
pub mod xml;

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::State;
use http::{Request, Response, StatusCode};

use crate::credentials::Action;
use crate::metadata::queries::BucketRow;
use crate::store::Store;
use auth::{AuthContext, AuthParams};
use capabilities::Op;
use error::{S3Error, S3Result};
use request::S3Request;

/// Per-request context passed to operation handlers.
pub struct Cx {
    pub store: Arc<Store>,
    pub req: S3Request,
    pub auth: AuthContext,
    pub op: Op,
}

impl Cx {
    pub fn credential_id(&self) -> &str {
        &self.auth.credential.id
    }

    pub fn require_object(&self, action: Action) -> S3Result<()> {
        let key = self.req.object_key();
        if self
            .auth
            .credential
            .allows_object(self.req.bucket_name(), key.as_bytes(), action)
        {
            Ok(())
        } else {
            Err(S3Error::access_denied())
        }
    }

    /// Resolve a bucket by name; invalid names cannot exist.
    pub async fn bucket(&self) -> S3Result<BucketRow> {
        lookup_bucket(&self.store, self.req.bucket_name()).await
    }

    pub fn idle_timeout(&self) -> Duration {
        Duration::from_secs(self.store.config.http.body_idle_timeout_seconds)
    }
}

pub async fn lookup_bucket(store: &Arc<Store>, name: &str) -> S3Result<BucketRow> {
    if crate::keys::validate_bucket_name(name).is_err() {
        return Err(S3Error::no_such_bucket());
    }
    let n = name.to_string();
    store
        .db
        .read(move |c| crate::metadata::queries::bucket_by_name(c, &n))
        .await?
        .ok_or_else(S3Error::no_such_bucket)
}

fn host_id() -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(crate::ids::random_bytes::<24>())
}

/// Serialize an S3 error. HEAD responses carry no body.
pub fn error_response(err: &S3Error, request_id: &str, head: bool) -> Response<Body> {
    let mut headers: Vec<(&str, String)> = vec![
        ("x-amz-request-id", request_id.to_string()),
        ("x-amz-id-2", host_id()),
    ];
    for (k, v) in &err.headers {
        headers.push((k, v.clone()));
    }
    if head {
        return headers::response(err.status, headers, Body::empty());
    }
    let mut w = xml::XmlWriter::new();
    w.open("Error")
        .elem("Code", err.code)
        .elem("Message", &err.message);
    for (k, v) in &err.extra {
        w.elem(k, v);
    }
    w.elem("RequestId", request_id).close("Error");
    headers.push(("content-type", "application/xml".into()));
    headers::response(err.status, headers, Body::from(w.finish()))
}

/// Axum fallback handler for every S3 request.
pub async fn handle(State(store): State<Arc<Store>>, request: Request<Body>) -> Response<Body> {
    let started = Instant::now();
    let request_id = crate::ids::request_id();
    let (parts, body) = request.into_parts();
    let head = parts.method == http::Method::HEAD;
    let origin = parts
        .headers
        .get(http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let mut op_name = "Unknown";
    let mut credential = String::new();
    let mut payload_mode = "";
    let mut bucket_name: Option<String> = None;
    let mut key_for_log: Option<String> = None;

    let result: S3Result<(Response<Body>, Op)> = async {
        let cfg = &store.config;
        let target_len = parts.uri.path().len() + parts.uri.query().map_or(0, |q| q.len() + 1);
        if target_len > cfg.http.max_request_target_bytes {
            return Err(S3Error::new(
                "RequestURITooLong",
                StatusCode::URI_TOO_LONG,
                "The request URI is too long",
            ));
        }
        let header_bytes: usize = parts
            .headers
            .iter()
            .map(|(k, v)| k.as_str().len() + v.len() + 4)
            .sum();
        if parts.headers.len() > cfg.http.max_header_count
            || header_bytes > cfg.http.max_header_bytes
        {
            return Err(S3Error::new(
                "RequestHeaderSectionTooLarge",
                StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
                "Your request header section exceeds the maximum allowed size.",
            ));
        }
        let req = S3Request::parse(
            request_id.clone(),
            parts.method.clone(),
            &parts.uri,
            parts.headers.clone(),
        )?;
        bucket_name = req.bucket.clone();
        if cfg.logging.log_object_keys {
            key_for_log = req.key.as_ref().map(|k| k.as_str().to_string());
        }
        auth::reject_unsupported_schemes(&req)?;
        let op = capabilities::resolve(&req)?;
        op_name = op.name();
        if !store.is_ready() {
            return Err(S3Error::service_unavailable(
                "The service is starting or shutting down.",
            ));
        }
        capabilities::validate(&req, op, &store.meta.owner_id)?;
        if op == Op::Preflight {
            return cors::preflight(&store, &req).await.map(|r| (r, op));
        }
        let now_ms = crate::metadata::now_ms();
        let creds = store.credentials.snapshot();
        let auth = auth::authenticate(
            &req,
            &creds,
            &AuthParams {
                region: &store.meta.region,
                max_skew_secs: cfg.http.max_clock_skew_seconds as i64,
                now_secs: now_ms / 1000,
                now_ms,
            },
        )?;
        credential = auth.credential.id.clone();
        payload_mode = auth.payload.label();
        if op.is_mutation() {
            store.check_writable()?;
        }
        let cx = Cx {
            store: store.clone(),
            req,
            auth,
            op,
        };
        dispatch(cx, body).await.map(|r| (r, op))
    }
    .await;

    let (mut response, error) = match result {
        Ok((r, _)) => (r, None),
        Err(e) => {
            let r = error_response(&e, &request_id, head);
            (r, Some(e))
        }
    };
    let status = response.status();
    {
        let h = response.headers_mut();
        if !h.contains_key("x-amz-request-id")
            && let Ok(v) = http::HeaderValue::from_str(&request_id)
        {
            h.insert("x-amz-request-id", v);
        }
        h.insert(
            http::header::SERVER,
            http::HeaderValue::from_static("litebucket"),
        );
    }
    if let (Some(origin), Some(bucket)) = (origin, bucket_name.as_deref())
        && op_name != "CorsPreflight"
    {
        cors::apply_actual_request_headers(&store, bucket, &origin, &parts.method, &mut response)
            .await;
    }
    let elapsed = started.elapsed();
    store.metrics.observe(op_name, status.as_u16(), elapsed);
    let code = error.as_ref().map(|e| e.code).unwrap_or("");
    let detail = error
        .as_ref()
        .and_then(|e| e.detail.as_deref())
        .unwrap_or("");
    if status.is_server_error() {
        tracing::error!(
            event = "request",
            request_id = %request_id,
            operation = op_name,
            credential = %credential,
            status = status.as_u16(),
            code,
            detail,
            duration_ms = elapsed.as_millis() as u64,
            payload = payload_mode,
            key = key_for_log.as_deref().unwrap_or(""),
        );
    } else {
        tracing::info!(
            event = "request",
            request_id = %request_id,
            operation = op_name,
            credential = %credential,
            status = status.as_u16(),
            code,
            detail,
            duration_ms = elapsed.as_millis() as u64,
            payload = payload_mode,
            key = key_for_log.as_deref().unwrap_or(""),
        );
    }
    response
}

async fn dispatch(cx: Cx, body: Body) -> S3Result<Response<Body>> {
    match cx.op {
        Op::ListBuckets => bucket::list_buckets(&cx).await,
        Op::CreateBucket => bucket::create_bucket(&cx, body).await,
        Op::HeadBucket => bucket::head_bucket(&cx).await,
        Op::DeleteBucket => bucket::delete_bucket(&cx).await,
        Op::GetBucketLocation => bucket::get_bucket_location(&cx).await,
        Op::ListObjectsV2 => listing::list_objects_v2(&cx).await,
        Op::DeleteObjects => object::delete_objects(&cx, body).await,
        Op::ListMultipartUploads => listing::list_multipart_uploads(&cx).await,
        Op::PutBucketCors => cors::put_bucket_cors(&cx, body).await,
        Op::GetBucketCors => cors::get_bucket_cors(&cx).await,
        Op::DeleteBucketCors => cors::delete_bucket_cors(&cx).await,
        Op::PutObject => object::put_object(&cx, body).await,
        Op::CopyObject => object::copy_object(&cx).await,
        Op::GetObject => object::get_object(&cx, false).await,
        Op::HeadObject => object::get_object(&cx, true).await,
        Op::DeleteObject => object::delete_object(&cx).await,
        Op::CreateMultipartUpload => multipart::create(&cx).await,
        Op::UploadPart => multipart::upload_part(&cx, body).await,
        Op::CompleteMultipartUpload => multipart::complete(&cx, body).await,
        Op::AbortMultipartUpload => multipart::abort(&cx).await,
        Op::ListParts => multipart::list_parts(&cx).await,
        Op::Preflight => Err(S3Error::internal()),
    }
}

/// Read a bounded control body (XML), verifying payload signing and any
/// supplied integrity values.
pub async fn read_control_body(
    cx: &Cx,
    body: Body,
    limit: usize,
    require_integrity: bool,
) -> S3Result<bytes::Bytes> {
    let mut integrity = integrity::ChecksumRequest::parse(&cx.req, &cx.auth.payload)?;
    if cx.op == capabilities::Op::CompleteMultipartUpload {
        // On completion, x-amz-checksum-* headers describe the assembled
        // object, not the XML body.
        integrity.header = None;
        integrity.sdk = None;
    }
    if require_integrity && integrity.content_md5.is_none() && integrity.algorithm().is_none() {
        return Err(S3Error::invalid_request(
            "Missing required header for this request: Content-MD5 or x-amz-checksum-*",
        ));
    }
    let expected = payload_length(&cx.req, &cx.auth.payload)?;
    let trailer = cx
        .req
        .header("x-amz-trailer")?
        .map(|t| t.trim().to_ascii_lowercase());
    let mut p = payload::Payload::new(
        body,
        cx.auth.payload.clone(),
        Some(cx.auth.signing.clone()),
        expected,
        trailer,
        cx.idle_timeout(),
    );
    let bytes = p.collect(limit).await?;
    let mut hashes = crate::checksums::BodyHashes::new(&integrity.algorithms(&[]));
    hashes.update(&bytes);
    integrity.verify(&hashes.finish(), p.trailers())?;
    Ok(bytes)
}

/// Decoded payload length: `x-amz-decoded-content-length` for aws-chunked
/// bodies, otherwise `Content-Length`.
pub fn payload_length(req: &S3Request, decl: &auth::PayloadDecl) -> S3Result<Option<u64>> {
    if decl.is_streaming() {
        let v = req
            .header("x-amz-decoded-content-length")?
            .ok_or_else(S3Error::missing_content_length)?;
        return v
            .parse()
            .map(Some)
            .map_err(|_| S3Error::invalid_argument("invalid x-amz-decoded-content-length"));
    }
    match req.content_length()? {
        Some(n) => Ok(Some(n)),
        // HTTP/1.1: no Content-Length and no Transfer-Encoding means an empty body.
        None if !req.headers.contains_key(http::header::TRANSFER_ENCODING) => Ok(Some(0)),
        None => Err(S3Error::missing_content_length()),
    }
}
