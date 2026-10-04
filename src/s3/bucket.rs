//! Bucket operations: ListBuckets, CreateBucket, HeadBucket, DeleteBucket,
//! GetBucketLocation.

use axum::body::Body;
use http::{Response, StatusCode};

use super::error::{S3Error, S3Result};
use super::headers::{iso8601, response, xml};
use super::listing::{self, TokenScope};
use super::xml::XmlWriter;
use super::{Cx, read_control_body};
use crate::credentials::BucketScope;
use crate::metadata::queries::{self, CreateBucket, DeleteBucket};
use crate::metadata::{now_ms, with_write_tx};

pub async fn list_buckets(cx: &Cx) -> S3Result<Response<Body>> {
    let scope = match cx.auth.credential.bucket_listing_scope() {
        BucketScope::All => None,
        BucketScope::Only(set) => Some(set),
        BucketScope::Denied => return Err(S3Error::access_denied()),
    };
    let max: usize = match cx.req.q("max-buckets") {
        Some(v) => v
            .parse()
            .ok()
            .filter(|n| (1..=10_000).contains(n))
            .ok_or_else(|| S3Error::invalid_argument("max-buckets must be between 1 and 10000"))?,
        None => usize::MAX,
    };
    let prefix = cx.req.q("prefix").unwrap_or("").to_string();
    if let Some(region) = cx.req.q("bucket-region")
        && region != cx.store.meta.region
    {
        // Every bucket lives in the configured region.
        return Ok(xml(
            StatusCode::OK,
            list_buckets_xml(cx, &[], None, &prefix),
        ));
    }
    let token_scope = TokenScope::buckets(&prefix, cx.credential_id());
    let mut after = match cx.req.q("continuation-token") {
        Some(t) => listing::decode_bucket_token(&cx.store, t, &token_scope)?,
        None => String::new(),
    };
    let mut out = Vec::new();
    let mut next = None;
    loop {
        let (a, p) = (after.clone(), prefix.clone());
        let batch = cx
            .store
            .db
            .read(move |c| queries::list_buckets(c, &a, &p, 1000))
            .await?;
        if batch.is_empty() {
            break;
        }
        after = batch.last().map(|b| b.name.clone()).unwrap_or_default();
        for b in batch {
            if scope.as_ref().is_some_and(|s| !s.contains(&b.name)) {
                continue;
            }
            if out.len() == max {
                next = Some(
                    out.last()
                        .map(|x: &queries::BucketRow| x.name.clone())
                        .unwrap_or_default(),
                );
                break;
            }
            out.push(b);
        }
        if next.is_some() {
            break;
        }
    }
    let token = next.map(|name| listing::encode_bucket_token(&cx.store, &name, &token_scope));
    Ok(xml(
        StatusCode::OK,
        list_buckets_xml(cx, &out, token.as_deref(), &prefix),
    ))
}

fn list_buckets_xml(
    cx: &Cx,
    buckets: &[queries::BucketRow],
    token: Option<&str>,
    prefix: &str,
) -> String {
    let mut w = XmlWriter::new();
    w.root("ListAllMyBucketsResult")
        .open("Owner")
        .elem("ID", &cx.store.meta.owner_id)
        .elem("DisplayName", "storlite")
        .close("Owner")
        .open("Buckets");
    for b in buckets {
        w.open("Bucket")
            .elem("Name", &b.name)
            .elem("CreationDate", &iso8601(b.created_at_ms))
            .elem("BucketRegion", &cx.store.meta.region)
            .close("Bucket");
    }
    w.close("Buckets").opt("ContinuationToken", token);
    if cx.req.has_q("prefix") {
        w.elem("Prefix", prefix);
    }
    w.close("ListAllMyBucketsResult");
    w.finish()
}

pub async fn create_bucket(cx: &Cx, body: Body) -> S3Result<Response<Body>> {
    if !cx.auth.credential.allows_create_bucket() {
        return Err(S3Error::access_denied());
    }
    let name = cx.req.bucket_name().to_string();
    crate::keys::validate_bucket_name(&name).map_err(S3Error::invalid_bucket_name)?;
    let limit = cx.store.config.limits.max_xml_body_bytes;
    let body = read_control_body(cx, body, limit, false).await?;
    let region = &cx.store.meta.region;
    let constraint = if body.iter().all(|b| b.is_ascii_whitespace()) {
        None
    } else {
        let doc = super::xml::parse(&body)?;
        if doc.name != "CreateBucketConfiguration" {
            return Err(S3Error::malformed_xml());
        }
        for c in &doc.children {
            if c.name != "LocationConstraint" {
                return Err(S3Error::not_implemented(format!(
                    "CreateBucketConfiguration element {} is not supported",
                    c.name
                )));
            }
        }
        doc.child_text("LocationConstraint")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let ok = match (&constraint, region.as_str()) {
        (None, "us-east-1") => true,
        (Some(_), "us-east-1") => false,
        (Some(c), r) => c == r,
        (None, _) => false,
    };
    if !ok {
        let msg = match &constraint {
            None => "The unspecified location constraint is incompatible for the region specific endpoint this request was sent to.".to_string(),
            Some(c) => format!("The {c} location constraint is incompatible for the region specific endpoint this request was sent to."),
        };
        return Err(S3Error::new(
            "IllegalLocationConstraintException",
            StatusCode::BAD_REQUEST,
            msg,
        )
        .with_extra("Region", region.clone()));
    }
    let max = cx.store.config.limits.max_buckets;
    let now = now_ms();
    let n = name.clone();
    let res = cx
        .store
        .db
        .write(move |c| with_write_tx(c, |tx| queries::create_bucket(tx, &n, now, max)))
        .await?;
    match res {
        CreateBucket::Created(_) => Ok(response(
            StatusCode::OK,
            vec![("location", format!("/{name}"))],
            Body::empty(),
        )),
        CreateBucket::AlreadyExists => Err(S3Error::bucket_already_owned()),
        CreateBucket::TooManyBuckets => Err(S3Error::too_many_buckets()),
    }
}

fn require_bucket_visibility(cx: &Cx) -> S3Result<()> {
    if cx.auth.credential.has_any_grant(cx.req.bucket_name()) {
        Ok(())
    } else {
        Err(S3Error::access_denied())
    }
}

pub async fn head_bucket(cx: &Cx) -> S3Result<Response<Body>> {
    require_bucket_visibility(cx)?;
    cx.bucket().await?;
    Ok(response(
        StatusCode::OK,
        vec![
            ("x-amz-bucket-region", cx.store.meta.region.clone()),
            ("x-amz-access-point-alias", "false".into()),
        ],
        Body::empty(),
    ))
}

pub async fn get_bucket_location(cx: &Cx) -> S3Result<Response<Body>> {
    require_bucket_visibility(cx)?;
    cx.bucket().await?;
    let region = &cx.store.meta.region;
    let value = if region == "us-east-1" {
        ""
    } else {
        region.as_str()
    };
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><LocationConstraint xmlns=\"{}\">{}</LocationConstraint>",
        super::xml::S3_NS,
        super::xml::escape(value)
    );
    Ok(xml(StatusCode::OK, body))
}

pub async fn delete_bucket(cx: &Cx) -> S3Result<Response<Body>> {
    if !cx
        .auth
        .credential
        .allows_manage_bucket(cx.req.bucket_name())
    {
        return Err(S3Error::access_denied());
    }
    let bucket = cx.bucket().await?;
    let id = bucket.id;
    let res = cx
        .store
        .db
        .write(move |c| with_write_tx(c, |tx| queries::delete_bucket(tx, &id)))
        .await?;
    match res {
        DeleteBucket::Deleted => Ok(response(
            StatusCode::NO_CONTENT,
            Vec::<(&str, String)>::new(),
            Body::empty(),
        )),
        DeleteBucket::NoSuchBucket => Err(S3Error::no_such_bucket()),
        DeleteBucket::NotEmpty => Err(S3Error::bucket_not_empty()),
    }
}
