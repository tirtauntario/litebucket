//! Validated random identifiers. `StorageId` is the only input to physical paths.

use std::fmt;

/// Fill a buffer from the operating system's CSPRNG.
pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    getrandom::fill(&mut buf).expect("operating system random source unavailable");
    buf
}

/// Parse exactly 32 lowercase hexadecimal characters into 16 bytes.
fn parse_hex16(s: &str) -> Option<[u8; 16]> {
    if s.len() != 32
        || !s
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    let mut out = [0u8; 16];
    hex::decode_to_slice(s, &mut out).ok()?;
    Some(out)
}

macro_rules! id16 {
    ($name:ident) => {
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name([u8; 16]);

        impl $name {
            pub fn random() -> Self {
                Self(random_bytes())
            }

            pub fn from_slice(bytes: &[u8]) -> Option<Self> {
                bytes.try_into().ok().map(Self)
            }

            pub fn parse_hex(s: &str) -> Option<Self> {
                parse_hex16(s).map(Self)
            }

            pub fn as_bytes(&self) -> &[u8; 16] {
                &self.0
            }

            pub fn to_hex(&self) -> String {
                hex::encode(self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self.to_hex())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.to_hex())
            }
        }
    };
}

id16!(StorageId);

impl StorageId {
    /// New storage ID. With the `failpoints` feature, IDs listed in
    /// `STORLITE_FORCE_STORAGE_IDS` (comma-separated hex) are handed out first
    /// so tests can inject collisions.
    pub fn allocate() -> Self {
        #[cfg(feature = "failpoints")]
        {
            use std::sync::{Mutex, OnceLock};
            static FORCED: OnceLock<Mutex<Vec<StorageId>>> = OnceLock::new();
            let q = FORCED.get_or_init(|| {
                let mut v: Vec<StorageId> = std::env::var("STORLITE_FORCE_STORAGE_IDS")
                    .unwrap_or_default()
                    .split(',')
                    .filter_map(StorageId::parse_hex)
                    .collect();
                v.reverse();
                Mutex::new(v)
            });
            if let Some(id) = q.lock().unwrap_or_else(|e| e.into_inner()).pop() {
                return id;
            }
        }
        Self::random()
    }
}
id16!(BucketId);
id16!(GenerationId);
id16!(StoreId);

/// Multipart upload ID: 32 lowercase hexadecimal characters (128 random bits).
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UploadId(String);

impl UploadId {
    pub fn random() -> Self {
        Self(hex::encode(random_bytes::<16>()))
    }

    pub fn parse(s: &str) -> Option<Self> {
        parse_hex16(s).map(|_| Self(s.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for UploadId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "UploadId({})", self.0)
    }
}

impl fmt::Display for UploadId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Short random request identifier (hex), returned as `x-amz-request-id`.
pub fn request_id() -> String {
    hex::encode_upper(random_bytes::<8>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_id_hex_roundtrip() {
        let id = StorageId::random();
        let hex = id.to_hex();
        assert_eq!(hex.len(), 32);
        assert_eq!(StorageId::parse_hex(&hex), Some(id));
    }

    #[test]
    fn rejects_malformed_ids() {
        for bad in [
            "",
            "../../etc/passwd",
            "A1B2C3D4E5F60718293A4B5C6D7E8F90",
            "a1b2c3d4e5f60718293a4b5c6d7e8f9",
            "a1b2c3d4e5f60718293a4b5c6d7e8f900",
            "a1b2c3d4e5f60718293a4b5c6d7e8f9g",
            "a1/2c3d4e5f60718293a4b5c6d7e8f90",
        ] {
            assert!(StorageId::parse_hex(bad).is_none(), "{bad}");
            assert!(UploadId::parse(bad).is_none(), "{bad}");
        }
        assert!(StorageId::from_slice(&[0u8; 15]).is_none());
    }
}
