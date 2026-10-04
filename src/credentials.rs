//! Operator-managed credential file: parsing, validation, permission model,
//! atomic reload snapshots, and secret generation.

use std::collections::{BTreeSet, HashMap};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::sync::{Arc, RwLock};

use serde::Deserialize;
use time::OffsetDateTime;

use crate::error::{Error, Result};
use crate::keys::validate_bucket_name;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialsFile {
    #[serde(default)]
    credentials: Vec<CredentialEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialEntry {
    id: String,
    secret_access_key: String,
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    expires_at: Option<String>,
    #[serde(default)]
    global_grants: Vec<String>,
    #[serde(default)]
    grants: Vec<GrantEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantEntry {
    bucket: String,
    #[serde(default)]
    prefix: String,
    actions: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Action {
    Read,
    List,
    Write,
    Delete,
    ManageBucket,
}

impl Action {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "read" => Self::Read,
            "list" => Self::List,
            "write" => Self::Write,
            "delete" => Self::Delete,
            "manage_bucket" => Self::ManageBucket,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GlobalGrant {
    Admin,
    ListBuckets,
    CreateBucket,
}

impl GlobalGrant {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "admin" => Self::Admin,
            "list_buckets" => Self::ListBuckets,
            "create_bucket" => Self::CreateBucket,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone)]
pub struct Grant {
    pub bucket: String,
    pub prefix: Vec<u8>,
    pub actions: BTreeSet<Action>,
}

/// One enabled credential. The secret is kept in memory because SigV4
/// verification requires the shared secret, not a one-way hash.
#[derive(Clone)]
pub struct Credential {
    pub id: String,
    secret: String,
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
    pub fn secret(&self) -> &str {
        &self.secret
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

/// Immutable validated credential set.
#[derive(Debug, Default)]
pub struct CredentialSet {
    by_id: HashMap<String, Arc<Credential>>,
    pub disabled_count: usize,
}

impl CredentialSet {
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

    /// Load from a file, enforcing owner-only permissions.
    pub fn load(path: &Path, allow_group_read: bool) -> Result<Self> {
        let meta = std::fs::symlink_metadata(path).map_err(|e| {
            Error::config(format!(
                "cannot stat credentials file {}: {e}",
                path.display()
            ))
        })?;
        let meta = if meta.file_type().is_symlink() {
            // Secret mounts commonly use symlinks; validate the target.
            std::fs::metadata(path)
                .map_err(|e| Error::config(format!("cannot stat credentials file target: {e}")))?
        } else {
            meta
        };
        if !meta.is_file() {
            return Err(Error::config("credentials file is not a regular file"));
        }
        let mode = meta.mode() & 0o777;
        let forbidden = if allow_group_read { 0o027 } else { 0o077 };
        if mode & forbidden != 0 {
            return Err(Error::config(format!(
                "credentials file permissions {mode:o} are too broad; use mode 0600"
            )));
        }
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::config(format!("cannot read credentials file: {e}")))?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self> {
        let file: CredentialsFile = toml::from_str(text).map_err(|e| {
            // toml errors can quote source lines; never echo secret material.
            Error::config(format!(
                "invalid credentials file at {}",
                e.span()
                    .map(|s| format!("byte {}", s.start))
                    .unwrap_or_default()
            ))
        })?;
        let mut set = CredentialSet::default();
        let mut seen = BTreeSet::new();
        for entry in file.credentials {
            validate_access_key_id(&entry.id)?;
            if !seen.insert(entry.id.clone()) {
                return Err(Error::config(format!(
                    "duplicate credential id {}",
                    entry.id
                )));
            }
            let mut global = BTreeSet::new();
            for g in &entry.global_grants {
                global.insert(GlobalGrant::parse(g).ok_or_else(|| {
                    Error::config(format!("credential {}: unknown global grant {g}", entry.id))
                })?);
            }
            let mut grants = Vec::new();
            for g in entry.grants {
                validate_bucket_name(&g.bucket).map_err(|e| {
                    Error::config(format!(
                        "credential {}: grant bucket {}: {e}",
                        entry.id, g.bucket
                    ))
                })?;
                if g.prefix.len() > crate::keys::MAX_KEY_BYTES {
                    return Err(Error::config(format!(
                        "credential {}: grant prefix too long",
                        entry.id
                    )));
                }
                if g.actions.is_empty() {
                    return Err(Error::config(format!(
                        "credential {}: grant has no actions",
                        entry.id
                    )));
                }
                let mut actions = BTreeSet::new();
                for a in &g.actions {
                    actions.insert(Action::parse(a).ok_or_else(|| {
                        Error::config(format!("credential {}: unknown action {a}", entry.id))
                    })?);
                }
                if actions.contains(&Action::ManageBucket) && !g.prefix.is_empty() {
                    return Err(Error::config(format!(
                        "credential {}: manage_bucket requires an empty (whole-bucket) prefix",
                        entry.id
                    )));
                }
                grants.push(Grant {
                    bucket: g.bucket,
                    prefix: g.prefix.into_bytes(),
                    actions,
                });
            }
            let expires_at_ms = match &entry.expires_at {
                Some(s) => Some(parse_rfc3339_ms(s).ok_or_else(|| {
                    Error::config(format!("credential {}: invalid expires_at", entry.id))
                })?),
                None => None,
            };
            if !entry.enabled {
                set.disabled_count += 1;
                continue;
            }
            validate_secret(&entry.id, &entry.secret_access_key)?;
            set.by_id.insert(
                entry.id.clone(),
                Arc::new(Credential {
                    id: entry.id,
                    secret: entry.secret_access_key,
                    expires_at_ms,
                    global,
                    grants,
                }),
            );
        }
        Ok(set)
    }
}

fn parse_rfc3339_ms(s: &str) -> Option<i64> {
    let t = OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok()?;
    i64::try_from(t.unix_timestamp_nanos() / 1_000_000).ok()
}

fn validate_access_key_id(id: &str) -> Result<()> {
    let ok = (3..=128).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.');
    if ok {
        Ok(())
    } else {
        Err(Error::config(
            "credential ids must be 3-128 characters of letters, digits, '-', '_' or '.'",
        ))
    }
}

fn validate_secret(id: &str, secret: &str) -> Result<()> {
    if secret.starts_with("REPLACE_WITH") {
        return Err(Error::config(format!(
            "credential {id} is enabled but still has a placeholder secret"
        )));
    }
    if secret.len() < 32 || secret.len() > 256 || !secret.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(Error::config(format!(
            "credential {id}: secret must be 32-256 printable ASCII characters (use `storlite credentials generate`)"
        )));
    }
    Ok(())
}

/// Shared, atomically replaceable credential snapshot.
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

/// Write a new disabled credential fragment with a 256-bit secret to an
/// exclusively created mode-0600 file. The secret is never printed.
pub fn generate(id: &str, output: &Path) -> Result<()> {
    validate_access_key_id(id)?;
    use base64::Engine;
    let secret =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(crate::ids::random_bytes::<32>());
    let text = format!(
        "# Generated by `storlite credentials generate`. Merge into the credentials file,\n\
         # add grants, set enabled = true, and keep the file mode 0600.\n\
         [[credentials]]\n\
         id = \"{id}\"\n\
         secret_access_key = \"{secret}\"\n\
         enabled = false\n\
         global_grants = []\n"
    );
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(output)
        .map_err(|e| Error::config(format!("cannot create {}: {e}", output.display())))?;
    f.write_all(text.as_bytes())?;
    f.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = include_str!("../docs/examples/credentials.example.toml");

    #[test]
    fn example_file_has_no_usable_credentials() {
        let set = CredentialSet::parse(EXAMPLE).unwrap();
        assert_eq!(set.enabled_count(), 0);
        assert_eq!(set.disabled_count, 3);
    }

    #[test]
    fn enabled_placeholder_is_rejected() {
        let text = EXAMPLE.replacen("enabled = false", "enabled = true", 1);
        assert!(CredentialSet::parse(&text).is_err());
    }

    fn sample() -> CredentialSet {
        CredentialSet::parse(
            r#"
[[credentials]]
id = "reader"
secret_access_key = "0123456789abcdef0123456789abcdef"
enabled = true
[[credentials.grants]]
bucket = "documents"
prefix = "customers/123/"
actions = ["read", "list"]

[[credentials]]
id = "admin-key"
secret_access_key = "0123456789abcdef0123456789abcdeX"
enabled = true
global_grants = ["admin"]
"#,
        )
        .unwrap()
    }

    #[test]
    fn prefix_scoped_permissions() {
        let set = sample();
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
    fn manage_bucket_requires_whole_bucket_prefix() {
        let text = r#"
[[credentials]]
id = "m"
secret_access_key = "0123456789abcdef0123456789abcdef"
enabled = true
[[credentials.grants]]
bucket = "documents"
prefix = "x/"
actions = ["manage_bucket"]
"#;
        assert!(CredentialSet::parse(text).is_err());
    }

    #[test]
    fn parse_errors_do_not_echo_secrets() {
        let text =
            "[[credentials]]\nid = \"abc\"\nsecret_access_key = \"SUPERSECRETVALUE\"\nbogus = 1\n";
        let err = CredentialSet::parse(text).unwrap_err().to_string();
        assert!(!err.contains("SUPERSECRETVALUE"), "{err}");
    }

    #[test]
    fn generate_creates_exclusive_private_file() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("k.toml");
        generate("app-key", &out).unwrap();
        let meta = std::fs::metadata(&out).unwrap();
        assert_eq!(meta.mode() & 0o777, 0o600);
        assert!(generate("app-key", &out).is_err(), "must not overwrite");
        let text = std::fs::read_to_string(&out)
            .unwrap()
            .replace("enabled = false", "enabled = true");
        let set = CredentialSet::parse(&text).unwrap();
        assert_eq!(set.get("app-key").unwrap().secret().len(), 43);
    }
}
