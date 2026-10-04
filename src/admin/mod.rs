//! Administration of access keys, grants, and buckets.
//!
//! `ops` functions are synchronous state transitions on one metadata
//! transaction: they validate, mutate, and append an audit record together,
//! so a change and its audit entry commit or roll back as one. The Unix-socket
//! API (`api`) runs them on the metadata writer and then refreshes the
//! in-memory credential snapshot; offline commands (`init`, `admin recover`)
//! run them on a locked offline connection.

pub mod api;
pub mod client;
pub mod commands;

use std::collections::BTreeSet;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::credentials::{
    self, Action, Credential, CredentialSet, GlobalGrant, Grant, format_rfc3339_ms,
    parse_rfc3339_ms,
};
use crate::error::{Error, Result};
use crate::ids::StoreId;
use crate::metadata::queries::{self, CreateBucket, CredentialRow, DeleteBucket, GrantRow};
use crate::s3::cors::{self, CorsRule};
use crate::secrets::SecretCodec;

/// Longest allowed rotation grace period (30 days).
pub const MAX_GRACE_SECONDS: u64 = 30 * 24 * 3600;

// ---------------------------------------------------------------------------
// Wire types (JSON over the admin socket)

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantJson {
    pub bucket: String,
    #[serde(default)]
    pub prefix: String,
    pub actions: Vec<String>,
}

impl GrantJson {
    fn to_grant(&self) -> Result<Grant> {
        let mut actions = BTreeSet::new();
        for a in &self.actions {
            actions.insert(Action::parse(a).ok_or_else(|| {
                Error::config(format!(
                    "unknown action {a:?} (expected read, list, write, delete, manage_bucket)"
                ))
            })?);
        }
        let g = Grant {
            bucket: self.bucket.clone(),
            prefix: self.prefix.as_bytes().to_vec(),
            actions,
        };
        g.validate()?;
        Ok(g)
    }

    pub fn from_grant(g: &Grant) -> Self {
        Self {
            bucket: g.bucket.clone(),
            prefix: String::from_utf8_lossy(&g.prefix).into_owned(),
            actions: g.action_names().into_iter().map(String::from).collect(),
        }
    }
}

fn grant_row(g: &Grant) -> GrantRow {
    GrantRow {
        bucket: g.bucket.clone(),
        prefix: g.prefix.clone(),
        actions: g.action_names().into_iter().map(String::from).collect(),
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CreateKeyRequest {
    /// Generated (`SL` + 18 base32 characters) when omitted.
    #[serde(default)]
    pub access_key_id: Option<String>,
    #[serde(default)]
    pub description: String,
    /// Defaults to true.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// RFC 3339 UTC.
    #[serde(default)]
    pub expires_at: Option<String>,
    #[serde(default)]
    pub global_grants: Vec<String>,
    #[serde(default)]
    pub grants: Vec<GrantJson>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateKeyRequest {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub expires_at: Option<String>,
    /// Remove the expiry.
    #[serde(default)]
    pub clear_expiry: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RotateKeyRequest {
    /// How long the previous secret keeps working (0 = revoke immediately).
    #[serde(default)]
    pub grace_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoveGrantRequest {
    pub bucket: String,
    #[serde(default)]
    pub prefix: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyInfo {
    pub access_key_id: String,
    pub enabled: bool,
    pub description: String,
    pub created_at: String,
    pub updated_at: String,
    pub expires_at: Option<String>,
    pub previous_secret_valid_until: Option<String>,
    pub global_grants: Vec<String>,
    pub grants: Vec<GrantJson>,
}

impl KeyInfo {
    fn from_row(r: &CredentialRow, now_ms: i64) -> Self {
        Self {
            access_key_id: r.id.clone(),
            enabled: r.enabled,
            description: r.description.clone(),
            created_at: format_rfc3339_ms(r.created_at_ms),
            updated_at: format_rfc3339_ms(r.updated_at_ms),
            expires_at: r.expires_at_ms.map(format_rfc3339_ms),
            previous_secret_valid_until: r
                .previous
                .as_ref()
                .map(|(_, until)| *until)
                .filter(|until| *until > now_ms)
                .map(format_rfc3339_ms),
            global_grants: r.global.clone(),
            grants: r
                .grants
                .iter()
                .map(|g| GrantJson {
                    bucket: g.bucket.clone(),
                    prefix: String::from_utf8_lossy(&g.prefix).into_owned(),
                    actions: g.actions.clone(),
                })
                .collect(),
        }
    }
}

/// A newly created or rotated key. The secret appears only here, once.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssuedKey {
    pub access_key_id: String,
    pub secret_access_key: String,
    #[serde(default)]
    pub previous_secret_valid_until: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CreateBucketRequest {
    pub name: String,
    #[serde(default)]
    pub quota_bytes: Option<u64>,
    #[serde(default)]
    pub cors: Option<Vec<CorsRule>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaRequest {
    /// `null` clears the quota.
    pub quota_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorsRequest {
    pub rules: Vec<CorsRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BucketInfo {
    pub name: String,
    pub created_at: String,
    pub object_count: i64,
    pub logical_bytes: i64,
    pub quota_bytes: Option<i64>,
    pub cors: Option<Vec<CorsRule>>,
}

impl BucketInfo {
    fn from_row(b: &queries::BucketRow) -> Self {
        Self {
            name: b.name.clone(),
            created_at: format_rfc3339_ms(b.created_at_ms),
            object_count: b.object_count,
            logical_bytes: b.logical_bytes,
            quota_bytes: b.quota_bytes,
            cors: cors::load_rules(b.cors_json.as_deref()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusInfo {
    pub version: String,
    pub store_id: String,
    pub region: String,
    pub secret_protection: String,
    pub keys_enabled: usize,
    pub keys_disabled: usize,
    pub buckets: i64,
}

// ---------------------------------------------------------------------------
// Operations

/// Everything an operation needs besides the transaction.
pub struct Ctx<'a> {
    pub codec: &'a SecretCodec,
    pub store_id: &'a StoreId,
    pub actor: &'a str,
    pub now_ms: i64,
    pub max_buckets: u64,
}

fn audit(
    conn: &Connection,
    cx: &Ctx<'_>,
    action: &str,
    target: &str,
    detail: serde_json::Value,
) -> Result<()> {
    queries::insert_audit(conn, cx.now_ms, cx.actor, action, target, &detail)
}

fn parse_global(names: &[String]) -> Result<BTreeSet<GlobalGrant>> {
    names
        .iter()
        .map(|g| {
            GlobalGrant::parse(g).ok_or_else(|| {
                Error::config(format!(
                    "unknown global grant {g:?} (expected admin, list_buckets, create_bucket)"
                ))
            })
        })
        .collect()
}

fn parse_expiry(s: &str) -> Result<i64> {
    parse_rfc3339_ms(s).ok_or_else(|| {
        Error::config(format!(
            "invalid expires_at {s:?}; use RFC 3339, e.g. 2027-01-01T00:00:00Z"
        ))
    })
}

fn not_found_key(id: &str) -> Error {
    Error::NotFound(format!("no access key {id}"))
}

fn not_found_bucket(name: &str) -> Error {
    Error::NotFound(format!("no bucket {name}"))
}

/// Refuse a change that would leave no enabled, unexpired admin key. (An
/// operator can still recover offline with `storlite admin recover`.)
fn keep_an_admin(conn: &Connection, cx: &Ctx<'_>) -> Result<()> {
    if queries::usable_admin_count(conn, cx.now_ms)? == 0 {
        return Err(Error::Conflict(
            "refusing a change that leaves no enabled admin key".into(),
        ));
    }
    Ok(())
}

pub fn create_key(conn: &Connection, cx: &Ctx<'_>, req: &CreateKeyRequest) -> Result<IssuedKey> {
    let id = match &req.access_key_id {
        Some(id) => {
            credentials::validate_access_key_id(id)?;
            id.clone()
        }
        None => credentials::generate_access_key_id(),
    };
    if queries::credential_exists(conn, &id)? {
        return Err(Error::Conflict(format!("access key {id} already exists")));
    }
    credentials::validate_description(&req.description)?;
    let global = parse_global(&req.global_grants)?;
    let grants = req
        .grants
        .iter()
        .map(GrantJson::to_grant)
        .collect::<Result<Vec<_>>>()?;
    let expires = req.expires_at.as_deref().map(parse_expiry).transpose()?;
    let secret = credentials::generate_secret();
    let sealed = cx.codec.seal(cx.store_id, &id, &secret)?;
    let global_names: Vec<&str> = global.iter().map(|g| g.as_str()).collect();
    let grant_rows: Vec<GrantRow> = grants.iter().map(grant_row).collect();
    queries::insert_credential(
        conn,
        &queries::NewCredential {
            id: &id,
            secret: &sealed,
            enabled: req.enabled.unwrap_or(true),
            description: &req.description,
            expires_at_ms: expires,
            global: &global_names,
            grants: &grant_rows,
            now_ms: cx.now_ms,
        },
    )?;
    audit(
        conn,
        cx,
        "key.create",
        &id,
        serde_json::json!({
            "enabled": req.enabled.unwrap_or(true),
            "global_grants": global_names,
            "grants": grants.iter().map(GrantJson::from_grant).collect::<Vec<_>>(),
            "expires_at": req.expires_at,
        }),
    )?;
    Ok(IssuedKey {
        access_key_id: id,
        secret_access_key: secret,
        previous_secret_valid_until: None,
    })
}

pub fn list_keys(conn: &Connection, now_ms: i64) -> Result<Vec<KeyInfo>> {
    Ok(queries::load_credentials(conn)?
        .iter()
        .map(|r| KeyInfo::from_row(r, now_ms))
        .collect())
}

pub fn get_key(conn: &Connection, id: &str, now_ms: i64) -> Result<KeyInfo> {
    queries::load_credential(conn, id)?
        .map(|r| KeyInfo::from_row(&r, now_ms))
        .ok_or_else(|| not_found_key(id))
}

pub fn update_key(
    conn: &Connection,
    cx: &Ctx<'_>,
    id: &str,
    req: &UpdateKeyRequest,
) -> Result<KeyInfo> {
    if !queries::credential_exists(conn, id)? {
        return Err(not_found_key(id));
    }
    if req.clear_expiry && req.expires_at.is_some() {
        return Err(Error::config("use either expires_at or clear_expiry"));
    }
    if let Some(e) = req.enabled {
        queries::set_credential_enabled(conn, id, e, cx.now_ms)?;
    }
    if let Some(d) = &req.description {
        credentials::validate_description(d)?;
        queries::set_credential_description(conn, id, d, cx.now_ms)?;
    }
    if let Some(e) = &req.expires_at {
        queries::set_credential_expiry(conn, id, Some(parse_expiry(e)?), cx.now_ms)?;
    } else if req.clear_expiry {
        queries::set_credential_expiry(conn, id, None, cx.now_ms)?;
    }
    keep_an_admin(conn, cx)?;
    audit(
        conn,
        cx,
        "key.update",
        id,
        serde_json::json!({
            "enabled": req.enabled,
            "description_changed": req.description.is_some(),
            "expires_at": req.expires_at,
            "clear_expiry": req.clear_expiry,
        }),
    )?;
    get_key(conn, id, cx.now_ms)
}

pub fn delete_key(conn: &Connection, cx: &Ctx<'_>, id: &str) -> Result<()> {
    if !queries::delete_credential(conn, id)? {
        return Err(not_found_key(id));
    }
    keep_an_admin(conn, cx)?;
    audit(conn, cx, "key.delete", id, serde_json::json!({}))
}

pub fn rotate_key(
    conn: &Connection,
    cx: &Ctx<'_>,
    id: &str,
    req: &RotateKeyRequest,
) -> Result<IssuedKey> {
    if req.grace_seconds > MAX_GRACE_SECONDS {
        return Err(Error::config(format!(
            "grace period is longer than {} days",
            MAX_GRACE_SECONDS / 86_400
        )));
    }
    let row = queries::load_credential(conn, id)?.ok_or_else(|| not_found_key(id))?;
    let secret = credentials::generate_secret();
    let sealed = cx.codec.seal(cx.store_id, id, &secret)?;
    let until = cx
        .now_ms
        .saturating_add((req.grace_seconds as i64).saturating_mul(1000));
    let previous = (req.grace_seconds > 0).then_some((&row.secret, until));
    queries::rotate_credential(conn, id, &sealed, previous, cx.now_ms)?;
    audit(
        conn,
        cx,
        "key.rotate",
        id,
        serde_json::json!({ "grace_seconds": req.grace_seconds }),
    )?;
    Ok(IssuedKey {
        access_key_id: id.to_string(),
        secret_access_key: secret,
        previous_secret_valid_until: previous.map(|(_, u)| format_rfc3339_ms(u)),
    })
}

pub fn put_grant(conn: &Connection, cx: &Ctx<'_>, id: &str, g: &GrantJson) -> Result<KeyInfo> {
    let grant = g.to_grant()?;
    if !queries::credential_touched(conn, id, cx.now_ms)? {
        return Err(not_found_key(id));
    }
    queries::upsert_grant(conn, id, &grant_row(&grant))?;
    audit(
        conn,
        cx,
        "grant.put",
        id,
        serde_json::to_value(GrantJson::from_grant(&grant)).unwrap_or_default(),
    )?;
    get_key(conn, id, cx.now_ms)
}

pub fn remove_grant(
    conn: &Connection,
    cx: &Ctx<'_>,
    id: &str,
    req: &RemoveGrantRequest,
) -> Result<KeyInfo> {
    if !queries::credential_touched(conn, id, cx.now_ms)? {
        return Err(not_found_key(id));
    }
    if !queries::remove_grant(conn, id, &req.bucket, req.prefix.as_bytes())? {
        return Err(Error::NotFound(format!(
            "access key {id} has no grant on {}/{}",
            req.bucket, req.prefix
        )));
    }
    audit(
        conn,
        cx,
        "grant.remove",
        id,
        serde_json::json!({ "bucket": req.bucket, "prefix": req.prefix }),
    )?;
    get_key(conn, id, cx.now_ms)
}

pub fn put_global_grant(conn: &Connection, cx: &Ctx<'_>, id: &str, name: &str) -> Result<KeyInfo> {
    let g = parse_global(&[name.to_string()])?;
    if !queries::credential_touched(conn, id, cx.now_ms)? {
        return Err(not_found_key(id));
    }
    for g in g {
        queries::add_global_grant(conn, id, g.as_str())?;
    }
    audit(
        conn,
        cx,
        "global_grant.put",
        id,
        serde_json::json!({ "grant": name }),
    )?;
    get_key(conn, id, cx.now_ms)
}

pub fn remove_global_grant(
    conn: &Connection,
    cx: &Ctx<'_>,
    id: &str,
    name: &str,
) -> Result<KeyInfo> {
    parse_global(&[name.to_string()])?;
    if !queries::credential_touched(conn, id, cx.now_ms)? {
        return Err(not_found_key(id));
    }
    if !queries::remove_global_grant(conn, id, name)? {
        return Err(Error::NotFound(format!(
            "access key {id} has no global grant {name}"
        )));
    }
    keep_an_admin(conn, cx)?;
    audit(
        conn,
        cx,
        "global_grant.remove",
        id,
        serde_json::json!({ "grant": name }),
    )?;
    get_key(conn, id, cx.now_ms)
}

pub fn create_bucket(
    conn: &Connection,
    cx: &Ctx<'_>,
    req: &CreateBucketRequest,
) -> Result<BucketInfo> {
    crate::keys::validate_bucket_name(&req.name)
        .map_err(|e| Error::config(format!("bucket {}: {e}", req.name)))?;
    let cors_json = cors_json(req.cors.as_deref())?;
    match queries::create_bucket(conn, &req.name, cx.now_ms, cx.max_buckets)? {
        CreateBucket::Created(_) => {}
        CreateBucket::AlreadyExists => {
            return Err(Error::Conflict(format!(
                "bucket {} already exists",
                req.name
            )));
        }
        CreateBucket::TooManyBuckets => {
            return Err(Error::Conflict(format!(
                "the store already has the maximum of {} buckets",
                cx.max_buckets
            )));
        }
    }
    if req.quota_bytes.is_some() {
        queries::set_bucket_quota(conn, &req.name, req.quota_bytes)?;
    }
    if let Some(json) = &cors_json {
        let b =
            queries::bucket_by_name(conn, &req.name)?.ok_or_else(|| not_found_bucket(&req.name))?;
        queries::set_bucket_cors(conn, &b.id, Some(json))?;
    }
    audit(
        conn,
        cx,
        "bucket.create",
        &req.name,
        serde_json::json!({ "quota_bytes": req.quota_bytes, "cors_rules": req.cors.as_ref().map(Vec::len) }),
    )?;
    get_bucket(conn, &req.name)
}

fn cors_json(rules: Option<&[CorsRule]>) -> Result<Option<String>> {
    match rules {
        None => Ok(None),
        Some(r) => {
            cors::validate_rules(r).map_err(Error::config)?;
            serde_json::to_string(r)
                .map(Some)
                .map_err(|e| Error::other(e.to_string()))
        }
    }
}

pub fn list_buckets(conn: &Connection) -> Result<Vec<BucketInfo>> {
    let mut out = Vec::new();
    let mut after = String::new();
    loop {
        let batch = queries::list_buckets(conn, &after, "", 1000)?;
        let Some(last) = batch.last() else { break };
        after = last.name.clone();
        out.extend(batch.iter().map(BucketInfo::from_row));
    }
    Ok(out)
}

pub fn get_bucket(conn: &Connection, name: &str) -> Result<BucketInfo> {
    queries::bucket_by_name(conn, name)?
        .map(|b| BucketInfo::from_row(&b))
        .ok_or_else(|| not_found_bucket(name))
}

pub fn delete_bucket(conn: &Connection, cx: &Ctx<'_>, name: &str) -> Result<()> {
    let b = queries::bucket_by_name(conn, name)?.ok_or_else(|| not_found_bucket(name))?;
    match queries::delete_bucket(conn, &b.id)? {
        DeleteBucket::Deleted => audit(conn, cx, "bucket.delete", name, serde_json::json!({})),
        DeleteBucket::NoSuchBucket => Err(not_found_bucket(name)),
        DeleteBucket::NotEmpty => Err(Error::Conflict(format!(
            "bucket {name} is not empty (objects or open multipart uploads)"
        ))),
    }
}

pub fn set_quota(
    conn: &Connection,
    cx: &Ctx<'_>,
    name: &str,
    quota: Option<u64>,
) -> Result<BucketInfo> {
    if !queries::set_bucket_quota(conn, name, quota)? {
        return Err(not_found_bucket(name));
    }
    audit(
        conn,
        cx,
        "bucket.quota",
        name,
        serde_json::json!({ "quota_bytes": quota }),
    )?;
    get_bucket(conn, name)
}

pub fn set_cors(
    conn: &Connection,
    cx: &Ctx<'_>,
    name: &str,
    rules: Option<&[CorsRule]>,
) -> Result<BucketInfo> {
    let json = cors_json(rules)?;
    let b = queries::bucket_by_name(conn, name)?.ok_or_else(|| not_found_bucket(name))?;
    queries::set_bucket_cors(conn, &b.id, json.as_deref())?;
    audit(
        conn,
        cx,
        if rules.is_some() {
            "bucket.cors.put"
        } else {
            "bucket.cors.delete"
        },
        name,
        serde_json::json!({ "rules": rules.map(<[CorsRule]>::len) }),
    )?;
    get_bucket(conn, name)
}

// ---------------------------------------------------------------------------
// Loading, protection-mode conversion, bootstrap

/// Decode every key into the in-memory set used for authentication. Fails
/// when any stored secret cannot be read (wrong or missing master key, or an
/// altered record): the service never runs with a partial key set.
pub fn load_credential_set(
    conn: &Connection,
    codec: &SecretCodec,
    store_id: &StoreId,
    now_ms: i64,
) -> Result<CredentialSet> {
    let rows = queries::load_credentials(conn)?;
    let mut enabled = Vec::new();
    let mut disabled = 0;
    for r in rows {
        let secret = codec.open(store_id, &r.id, &r.secret)?;
        let previous = match &r.previous {
            Some((s, until)) if *until > now_ms => Some((codec.open(store_id, &r.id, s)?, *until)),
            _ => None,
        };
        if !r.enabled {
            disabled += 1;
            continue;
        }
        let global = r
            .global
            .iter()
            .filter_map(|g| GlobalGrant::parse(g))
            .collect();
        let grants = r
            .grants
            .iter()
            .map(|g| {
                GrantJson {
                    bucket: g.bucket.clone(),
                    prefix: String::from_utf8_lossy(&g.prefix).into_owned(),
                    actions: g.actions.clone(),
                }
                .to_grant()
            })
            .collect::<Result<Vec<_>>>()
            .map_err(|e| Error::integrity(format!("access key {}: {e}", r.id)))?;
        enabled.push(Credential::new(
            r.id,
            secret,
            previous,
            r.expires_at_ms,
            global,
            grants,
        ));
    }
    Ok(CredentialSet::new(enabled, disabled))
}

/// Re-encode every stored secret whose scheme differs from the configured
/// protection mode. Runs inside one transaction: all rows convert or none.
pub fn convert_secret_protection(
    conn: &Connection,
    codec: &SecretCodec,
    store_id: &StoreId,
) -> Result<usize> {
    let target = codec.target_scheme();
    let mut converted = 0;
    for r in queries::load_credentials(conn)? {
        let cur_ok = r.secret.scheme == target;
        let prev_ok = r.previous.as_ref().is_none_or(|(p, _)| p.scheme == target);
        if cur_ok && prev_ok {
            continue;
        }
        let secret = codec.seal(store_id, &r.id, &codec.open(store_id, &r.id, &r.secret)?)?;
        let previous = match &r.previous {
            Some((p, _)) => Some(codec.seal(store_id, &r.id, &codec.open(store_id, &r.id, p)?)?),
            None => None,
        };
        queries::reencode_credential(conn, &r.id, &secret, previous.as_ref())?;
        converted += 1;
    }
    Ok(converted)
}

/// Create an admin key (used by `init` and `admin recover`).
pub fn create_admin_key(conn: &Connection, cx: &Ctx<'_>, id: Option<String>) -> Result<IssuedKey> {
    create_key(
        conn,
        cx,
        &CreateKeyRequest {
            access_key_id: id,
            description: "admin key created by storlite".into(),
            enabled: Some(true),
            global_grants: vec!["admin".into()],
            ..Default::default()
        },
    )
}

/// Status summary for `GET /v1/status`.
pub fn status(
    conn: &Connection,
    meta: &queries::StoreMeta,
    codec: &SecretCodec,
    set: &CredentialSet,
) -> Result<StatusInfo> {
    Ok(StatusInfo {
        version: env!("CARGO_PKG_VERSION").into(),
        store_id: meta.store_id.to_hex(),
        region: meta.region.clone(),
        secret_protection: if codec.encrypts() {
            "encrypted".into()
        } else {
            "plaintext".into()
        },
        keys_enabled: set.enabled_count(),
        keys_disabled: set.disabled_count,
        buckets: conn.query_row("SELECT count(*) FROM buckets", [], |r| r.get(0))?,
    })
}
