//! Access keys: the authorization model, validation, key generation, and the
//! atomically replaceable in-memory snapshot used on every request.
//!
//! Keys are stored in the metadata database (see `metadata::queries` and
//! `admin`); this module never touches storage.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, RwLock};

use time::OffsetDateTime;

use crate::error::{Error, Result};
use crate::keys::validate_bucket_name;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Action {
    Read,
    List,
    Write,
    Delete,
    ManageBucket,
}

impl Action {
    pub const ALL: [Action; 5] = [
        Self::Read,
        Self::List,
        Self::Write,
        Self::Delete,
        Self::ManageBucket,
    ];

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "read" => Self::Read,
            "list" => Self::List,
            "write" => Self::Write,
            "delete" => Self::Delete,
            "manage_bucket" => Self::ManageBucket,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::List => "list",
            Self::Write => "write",
            Self::Delete => "delete",
            Self::ManageBucket => "manage_bucket",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GlobalGrant {
    Admin,
    ListBuckets,
    CreateBucket,
}

impl GlobalGrant {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "admin" => Self::Admin,
            "list_buckets" => Self::ListBuckets,
            "create_bucket" => Self::CreateBucket,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::ListBuckets => "list_buckets",
            Self::CreateBucket => "create_bucket",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub bucket: String,
    pub prefix: Vec<u8>,
    pub actions: BTreeSet<Action>,
}

impl Grant {
    /// Validate a grant: bucket name rules, prefix length, at least one
    /// action, and `manage_bucket` only on the whole bucket.
    pub fn validate(&self) -> Result<()> {
        validate_bucket_name(&self.bucket)
            .map_err(|e| Error::config(format!("grant bucket {}: {e}", self.bucket)))?;
        if self.prefix.len() > crate::keys::MAX_KEY_BYTES {
            return Err(Error::config("grant prefix is longer than 1024 bytes"));
        }
        if self.actions.is_empty() {
            return Err(Error::config("a grant needs at least one action"));
        }
        if self.actions.contains(&Action::ManageBucket) && !self.prefix.is_empty() {
            return Err(Error::config(
                "manage_bucket requires an empty (whole-bucket) prefix",
            ));
        }
        Ok(())
    }

    /// Parse `bucket[/prefix]:action[,action...]`. Bucket names cannot
    /// contain `/` and actions cannot contain `:`, so splitting at the first
    /// `/` and the last `:` is unambiguous (a prefix may contain `:`).
    pub fn parse_spec(spec: &str) -> Result<Self> {
        let (target, actions) = spec.rsplit_once(':').ok_or_else(|| {
            Error::config(format!(
                "invalid grant {spec:?}; expected bucket[/prefix]:action[,action...]"
            ))
        })?;
        let (bucket, prefix) = split_target(target);
        let mut set = BTreeSet::new();
        for a in actions.split(',').map(str::trim).filter(|a| !a.is_empty()) {
            set.insert(Action::parse(a).ok_or_else(|| {
                Error::config(format!(
                    "unknown action {a:?} (expected read, list, write, delete, manage_bucket)"
                ))
            })?);
        }
        let g = Grant {
            bucket: bucket.to_string(),
            prefix: prefix.as_bytes().to_vec(),
            actions: set,
        };
        g.validate()?;
        Ok(g)
    }

    pub fn action_names(&self) -> Vec<&'static str> {
        self.actions.iter().map(|a| a.as_str()).collect()
    }
}

/// Split `bucket[/prefix]` at the first `/`.
pub fn split_target(target: &str) -> (&str, &str) {
    match target.split_once('/') {
        Some((b, p)) => (b, p),
        None => (target, ""),
    }
}

/// One enabled credential. The secret is kept in memory because SigV4
/// verification requires the shared secret, not a one-way hash.
#[derive(Clone)]
pub struct Credential {
    pub id: String,
    secret: String,
    /// Previous secret, accepted until the given time (rotation grace).
    previous: Option<(String, i64)>,
    pub expires_at_ms: Option<i64>,
    pub global: BTreeSet<GlobalGrant>,
    pub grants: Vec<Grant>,
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential")
            .field("id", &self.id)
            .field("secret", &"<redacted>")
            .field("global", &self.global)
            .field("grants", &self.grants.len())
            .finish()
    }
}

impl Credential {
    pub fn new(
        id: String,
        secret: String,
        previous: Option<(String, i64)>,
        expires_at_ms: Option<i64>,
        global: BTreeSet<GlobalGrant>,
        grants: Vec<Grant>,
    ) -> Self {
        Self {
            id,
            secret,
            previous,
            expires_at_ms,
            global,
            grants,
        }
    }

    pub fn secret(&self) -> &str {
        &self.secret
    }

    /// Secrets that currently authenticate this key: the current one, plus
    /// the previous one while its rotation grace period lasts.
    pub fn valid_secrets(&self, now_ms: i64) -> impl Iterator<Item = &str> {
        std::iter::once(self.secret.as_str()).chain(
            self.previous
                .as_ref()
                .filter(|(_, until)| now_ms < *until)
                .map(|(s, _)| s.as_str()),
        )
    }

    pub fn is_expired(&self, now_ms: i64) -> bool {
        self.expires_at_ms.is_some_and(|e| now_ms >= e)
    }

    pub fn is_admin(&self) -> bool {
        self.global.contains(&GlobalGrant::Admin)
    }

    /// Object-level action on an exact key.
    pub fn allows_object(&self, bucket: &str, key: &[u8], action: Action) -> bool {
        self.is_admin()
            || self.grants.iter().any(|g| {
                g.bucket == bucket && key.starts_with(&g.prefix) && g.actions.contains(&action)
            })
    }

    /// Listing under a requested prefix: the requested prefix must lie inside a
    /// granted prefix. Filtering after pagination is never used.
    pub fn allows_list(&self, bucket: &str, prefix: &[u8]) -> bool {
        self.is_admin()
            || self.grants.iter().any(|g| {
                g.bucket == bucket
                    && prefix.starts_with(&g.prefix)
                    && g.actions.contains(&Action::List)
            })
    }

    /// Existence/region disclosure for HeadBucket and GetBucketLocation.
    pub fn has_any_grant(&self, bucket: &str) -> bool {
        self.is_admin() || self.grants.iter().any(|g| g.bucket == bucket)
    }

    /// Whole-bucket administration (DeleteBucket, CORS).
    pub fn allows_manage_bucket(&self, bucket: &str) -> bool {
        self.is_admin()
            || self.grants.iter().any(|g| {
                g.bucket == bucket
                    && g.prefix.is_empty()
                    && g.actions.contains(&Action::ManageBucket)
            })
    }

    pub fn allows_create_bucket(&self) -> bool {
        self.is_admin() || self.global.contains(&GlobalGrant::CreateBucket)
    }

    /// Which buckets ListBuckets may enumerate for this credential.
    pub fn bucket_listing_scope(&self) -> BucketScope {
        if self.is_admin() {
            BucketScope::All
        } else if self.global.contains(&GlobalGrant::ListBuckets) {
            BucketScope::Only(self.grants.iter().map(|g| g.bucket.clone()).collect())
        } else {
            BucketScope::Denied
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BucketScope {
    All,
    Only(BTreeSet<String>),
    Denied,
}

/// Immutable set of enabled credentials.
#[derive(Debug, Default)]
pub struct CredentialSet {
    by_id: HashMap<String, Arc<Credential>>,
    pub disabled_count: usize,
}

impl CredentialSet {
    pub fn new(enabled: Vec<Credential>, disabled_count: usize) -> Self {
        Self {
            by_id: enabled
                .into_iter()
                .map(|c| (c.id.clone(), Arc::new(c)))
                .collect(),
            disabled_count,
        }
    }

    pub fn get(&self, id: &str) -> Option<Arc<Credential>> {
        self.by_id.get(id).cloned()
    }

    pub fn enabled_count(&self) -> usize {
        self.by_id.len()
    }

    pub fn ids(&self) -> Vec<String> {
        let mut v: Vec<_> = self.by_id.keys().cloned().collect();
        v.sort();
        v
    }
}

/// Shared, atomically replaceable credential snapshot. Requests take a
/// snapshot once; a replacement applies to every later request.
#[derive(Debug, Clone)]
pub struct CredentialStore {
    inner: Arc<RwLock<Arc<CredentialSet>>>,
}

impl CredentialStore {
    pub fn new(set: CredentialSet) -> Self {
        Self {
            inner: Arc::new(RwLock::new(Arc::new(set))),
        }
    }

    pub fn snapshot(&self) -> Arc<CredentialSet> {
        self.inner.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn replace(&self, set: CredentialSet) {
        *self.inner.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(set);
    }
}

pub fn parse_rfc3339_ms(s: &str) -> Option<i64> {
    let t = OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok()?;
    i64::try_from(t.unix_timestamp_nanos() / 1_000_000).ok()
}

pub fn format_rfc3339_ms(ms: i64) -> String {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .ok()
        .and_then(|t| {
            t.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_default()
}

pub fn validate_access_key_id(id: &str) -> Result<()> {
    let ok = (3..=128).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.');
    if ok {
        Ok(())
    } else {
        Err(Error::config(
            "access key ids must be 3-128 characters of letters, digits, '-', '_' or '.'",
        ))
    }
}

pub fn validate_secret(secret: &str) -> Result<()> {
    if secret.len() < 32 || secret.len() > 256 || !secret.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(Error::config(
            "secret access keys must be 32-256 printable ASCII characters",
        ));
    }
    Ok(())
}

pub fn validate_description(d: &str) -> Result<()> {
    if d.len() > 256 || d.chars().any(char::is_control) {
        return Err(Error::config(
            "descriptions must be at most 256 bytes without control characters",
        ));
    }
    Ok(())
}

const BASE32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// A new access key id: `SL` + 18 base32 characters (90 random bits). Access
/// key ids are identifiers, not secrets; the format resembles AWS key ids so
/// tools that pattern-match them behave.
pub fn generate_access_key_id() -> String {
    let bytes = crate::ids::random_bytes::<18>();
    let mut id = String::with_capacity(20);
    id.push_str("SL");
    for b in bytes {
        id.push(BASE32[usize::from(b & 31)] as char);
    }
    id
}

/// A new secret access key: 256 bits from the OS CSPRNG, base64url (43 chars).
pub fn generate_secret() -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(crate::ids::random_bytes::<32>())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cred(id: &str, global: &[GlobalGrant], grants: Vec<Grant>) -> Credential {
        Credential::new(
            id.into(),
            generate_secret(),
            None,
            None,
            global.iter().copied().collect(),
            grants,
        )
    }

    #[test]
    fn prefix_scoped_permissions() {
        let set = CredentialSet::new(
            vec![
                cred(
                    "reader",
                    &[],
                    vec![Grant::parse_spec("documents/customers/123/:read,list").unwrap()],
                ),
                cred("admin-key", &[GlobalGrant::Admin], vec![]),
            ],
            0,
        );
        let r = set.get("reader").unwrap();
        assert!(r.allows_object("documents", b"customers/123/a.pdf", Action::Read));
        assert!(!r.allows_object("documents", b"customers/1234/a.pdf", Action::Read));
        assert!(!r.allows_object("documents", b"customers/123/a.pdf", Action::Write));
        assert!(!r.allows_object("other", b"customers/123/a.pdf", Action::Read));
        assert!(r.allows_list("documents", b"customers/123/"));
        assert!(r.allows_list("documents", b"customers/123/sub"));
        for p in [&b""[..], b"customers/", b"customers/1234/", b"customers/12"] {
            assert!(!r.allows_list("documents", p));
        }
        assert!(r.has_any_grant("documents"));
        assert!(!r.allows_manage_bucket("documents"));
        assert!(!r.allows_create_bucket());
        assert_eq!(r.bucket_listing_scope(), BucketScope::Denied);
        let a = set.get("admin-key").unwrap();
        assert!(a.allows_object("anything", b"x", Action::Delete));
        assert!(a.allows_create_bucket());
    }

    #[test]
    fn grant_specs() {
        let g = Grant::parse_spec("docs:read,write").unwrap();
        assert_eq!((g.bucket.as_str(), g.prefix.as_slice()), ("docs", &b""[..]));
        assert_eq!(g.action_names(), ["read", "write"]);
        let g = Grant::parse_spec("docs/a:b/c:list").unwrap();
        assert_eq!(g.prefix, b"a:b/c");
        assert!(Grant::parse_spec("docs/x/:manage_bucket").is_err());
        assert!(Grant::parse_spec("docs:manage_bucket").is_ok());
        assert!(Grant::parse_spec("docs:fly").is_err());
        assert!(Grant::parse_spec("docs:").is_err());
        assert!(Grant::parse_spec("Bad_Bucket:read").is_err());
        assert!(Grant::parse_spec("docs").is_err());
    }

    #[test]
    fn generated_keys() {
        let id = generate_access_key_id();
        assert_eq!(id.len(), 20);
        assert!(id.starts_with("SL"));
        assert!(id.bytes().skip(2).all(|b| BASE32.contains(&b)));
        validate_access_key_id(&id).unwrap();
        assert_ne!(id, generate_access_key_id());
        let s = generate_secret();
        assert_eq!(s.len(), 43);
        validate_secret(&s).unwrap();
        assert_ne!(s, generate_secret());
    }

    #[test]
    fn rotation_grace_accepts_previous_secret_until_deadline() {
        let c = Credential::new(
            "k".into(),
            "new".into(),
            Some(("old".into(), 1000)),
            None,
            BTreeSet::new(),
            vec![],
        );
        assert_eq!(c.valid_secrets(999).collect::<Vec<_>>(), ["new", "old"]);
        assert_eq!(c.valid_secrets(1000).collect::<Vec<_>>(), ["new"]);
    }

    #[test]
    fn timestamps_round_trip() {
        let ms = parse_rfc3339_ms("2027-01-01T00:00:00Z").unwrap();
        assert_eq!(format_rfc3339_ms(ms), "2027-01-01T00:00:00Z");
    }
}
