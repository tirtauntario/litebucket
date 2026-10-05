//! Protection of access-key secrets stored in the metadata database.
//!
//! SigV4 needs the raw shared secret, so secrets cannot be hashed. In the
//! default `encrypted` mode each secret is sealed with AES-256-GCM under a
//! 32-byte master key kept in a file outside the data directory. The
//! associated data binds every ciphertext to the store and the access key id,
//! so a sealed value cannot be moved to another row or another store. The
//! `plaintext` mode stores secrets as-is and relies on file permissions only.

use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use base64::Engine;
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};

use crate::error::{Error, Result};
use crate::ids::{StoreId, random_bytes};

/// Storage scheme recorded per secret.
pub const SCHEME_PLAINTEXT: i64 = 0;
pub const SCHEME_AES256GCM_V1: i64 = 1;

// Bound into every sealed secret; keeps the pre-rename name so existing
// encrypted keys still open.
const AAD_PREFIX: &[u8] = b"storlite-credential-secret-v1\0";

/// A secret as stored in the database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sealed {
    pub scheme: i64,
    pub nonce: Option<Vec<u8>>,
    pub value: Vec<u8>,
}

/// The 32-byte master key. Never printed or logged.
pub struct MasterKey {
    key: LessSafeKey,
}

impl std::fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MasterKey(<redacted>)")
    }
}

impl MasterKey {
    fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let unbound = UnboundKey::new(&AES_256_GCM, bytes)
            .map_err(|_| Error::config("master key must be 32 bytes"))?;
        Ok(Self {
            key: LessSafeKey::new(unbound),
        })
    }

    /// Load a master key file: one line of standard base64 encoding 32 bytes.
    /// The file must be a regular file (symlinks to one are followed, as
    /// secret mounts use them) that group and others cannot read.
    pub fn load(path: &Path) -> Result<Self> {
        let meta = std::fs::metadata(path).map_err(|e| {
            Error::config(format!(
                "cannot read master key file {}: {e}",
                path.display()
            ))
        })?;
        if !meta.is_file() {
            return Err(Error::config(format!(
                "master key file {} is not a regular file",
                path.display()
            )));
        }
        let mode = meta.mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(Error::config(format!(
                "master key file {} has mode {mode:o}; use 0600 or 0400",
                path.display()
            )));
        }
        let text = std::fs::read_to_string(path).map_err(|e| {
            Error::config(format!(
                "cannot read master key file {}: {e}",
                path.display()
            ))
        })?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(text.trim())
            .map_err(|_| Error::config("master key file is not valid base64"))?;
        if bytes.len() != 32 {
            return Err(Error::config(
                "master key file must contain exactly 32 bytes (base64 encoded)",
            ));
        }
        Self::from_bytes(&bytes)
    }

    /// Create a new master key file (exclusive create, mode 0600, synced).
    pub fn generate(path: &Path) -> Result<()> {
        let key = random_bytes::<32>();
        let text = format!(
            "{}\n",
            base64::engine::general_purpose::STANDARD.encode(key)
        );
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(path)
            .map_err(|e| Error::config(format!("cannot create {}: {e}", path.display())))?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            crate::fsutil::sync_dir(crate::fsutil::open_dir(parent)?)?;
        }
        Ok(())
    }
}

fn aad(store: &StoreId, access_key_id: &str) -> Vec<u8> {
    let mut a = Vec::with_capacity(AAD_PREFIX.len() + 16 + access_key_id.len());
    a.extend_from_slice(AAD_PREFIX);
    a.extend_from_slice(store.as_bytes());
    a.extend_from_slice(access_key_id.as_bytes());
    a
}

/// How new secrets are written, and which keys can read existing ones.
#[derive(Debug)]
pub struct SecretCodec {
    encrypt: bool,
    master: Option<MasterKey>,
}

impl SecretCodec {
    pub fn plaintext(master: Option<MasterKey>) -> Self {
        Self {
            encrypt: false,
            master,
        }
    }

    pub fn encrypted(master: MasterKey) -> Self {
        Self {
            encrypt: true,
            master: Some(master),
        }
    }

    pub fn encrypts(&self) -> bool {
        self.encrypt
    }

    /// The scheme new secrets are written with.
    pub fn target_scheme(&self) -> i64 {
        if self.encrypt {
            SCHEME_AES256GCM_V1
        } else {
            SCHEME_PLAINTEXT
        }
    }

    pub fn seal(&self, store: &StoreId, access_key_id: &str, secret: &str) -> Result<Sealed> {
        if !self.encrypt {
            return Ok(Sealed {
                scheme: SCHEME_PLAINTEXT,
                nonce: None,
                value: secret.as_bytes().to_vec(),
            });
        }
        let master = self
            .master
            .as_ref()
            .ok_or_else(|| Error::config("encrypted secrets require a master key"))?;
        let nonce_bytes = random_bytes::<12>();
        let mut buf = secret.as_bytes().to_vec();
        master
            .key
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce_bytes),
                Aad::from(aad(store, access_key_id)),
                &mut buf,
            )
            .map_err(|_| Error::other("secret encryption failed"))?;
        Ok(Sealed {
            scheme: SCHEME_AES256GCM_V1,
            nonce: Some(nonce_bytes.to_vec()),
            value: buf,
        })
    }

    pub fn open(&self, store: &StoreId, access_key_id: &str, sealed: &Sealed) -> Result<String> {
        let bytes = match sealed.scheme {
            SCHEME_PLAINTEXT => sealed.value.clone(),
            SCHEME_AES256GCM_V1 => {
                let master = self.master.as_ref().ok_or_else(|| {
                    Error::config(
                        "stored secrets are encrypted but no master_key_file is configured",
                    )
                })?;
                let nonce: [u8; 12] = sealed
                    .nonce
                    .as_deref()
                    .and_then(|n| n.try_into().ok())
                    .ok_or_else(|| Error::integrity("encrypted secret has an invalid nonce"))?;
                let mut buf = sealed.value.clone();
                let plain = master
                    .key
                    .open_in_place(
                        Nonce::assume_unique_for_key(nonce),
                        Aad::from(aad(store, access_key_id)),
                        &mut buf,
                    )
                    .map_err(|_| {
                        Error::config(format!(
                            "cannot decrypt the secret of access key {access_key_id}: wrong master key, or the record was altered"
                        ))
                    })?;
                plain.to_vec()
            }
            other => {
                return Err(Error::integrity(format!(
                    "access key {access_key_id} uses unknown secret scheme {other}"
                )));
            }
        };
        String::from_utf8(bytes)
            .map_err(|_| Error::integrity(format!("secret of {access_key_id} is not UTF-8")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> MasterKey {
        MasterKey::from_bytes(&[7u8; 32]).unwrap()
    }

    #[test]
    fn encrypted_round_trip_and_binding() {
        let store = StoreId::random();
        let codec = SecretCodec::encrypted(key());
        let sealed = codec.seal(&store, "SLKEY", "s3cret-value").unwrap();
        assert_eq!(sealed.scheme, SCHEME_AES256GCM_V1);
        assert!(!sealed.value.windows(6).any(|w| w == b"s3cret"));
        assert_eq!(
            codec.open(&store, "SLKEY", &sealed).unwrap(),
            "s3cret-value"
        );
        // Bound to the access key id and the store.
        assert!(codec.open(&store, "OTHER", &sealed).is_err());
        assert!(codec.open(&StoreId::random(), "SLKEY", &sealed).is_err());
        // Tampering is detected.
        let mut bad = sealed.clone();
        bad.value[0] ^= 1;
        assert!(codec.open(&store, "SLKEY", &bad).is_err());
        // A different master key cannot open it.
        let other = SecretCodec::encrypted(MasterKey::from_bytes(&[8u8; 32]).unwrap());
        assert!(other.open(&store, "SLKEY", &sealed).is_err());
        // Plaintext mode without a key cannot read encrypted rows.
        assert!(
            SecretCodec::plaintext(None)
                .open(&store, "SLKEY", &sealed)
                .is_err()
        );
    }

    #[test]
    fn nonces_are_unique() {
        let store = StoreId::random();
        let codec = SecretCodec::encrypted(key());
        let a = codec.seal(&store, "SLKEY", "x").unwrap();
        let b = codec.seal(&store, "SLKEY", "x").unwrap();
        assert_ne!(a.nonce, b.nonce);
        assert_ne!(a.value, b.value);
    }

    #[test]
    fn plaintext_round_trip() {
        let store = StoreId::random();
        let codec = SecretCodec::plaintext(None);
        let sealed = codec.seal(&store, "SLKEY", "abc").unwrap();
        assert_eq!(sealed.scheme, SCHEME_PLAINTEXT);
        assert_eq!(codec.open(&store, "SLKEY", &sealed).unwrap(), "abc");
    }

    #[test]
    fn key_file_generation_and_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("master.key");
        MasterKey::generate(&path).unwrap();
        assert!(MasterKey::generate(&path).is_err(), "never overwrites");
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        MasterKey::load(&path).unwrap();
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o644))
            .unwrap();
        assert!(
            MasterKey::load(&path).is_err(),
            "world-readable key refused"
        );
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .unwrap();
        std::fs::write(&path, "c2hvcnQ=\n").unwrap();
        assert!(MasterKey::load(&path).is_err(), "wrong length refused");
    }
}
