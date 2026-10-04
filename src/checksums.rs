//! S3 checksum algorithms, representations, and streaming hashers.
//!
//! Four integrity concepts are kept separate: SigV4 payload hashes (auth),
//! explicit S3 checksums (this module), ETags (MD5-based), and the internal
//! whole-file SHA-256 used by offline verification.

use base64::Engine;
use md5::Md5;
use serde::{Deserialize, Serialize};
use sha1::Sha1;
use sha2::{Digest, Sha256};

static CRC64_NVME: crc::Crc<u64> = crc::Crc::<u64>::new(&crc::CRC_64_NVME);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
pub enum Algorithm {
    #[serde(rename = "CRC32")]
    Crc32,
    #[serde(rename = "CRC32C")]
    Crc32c,
    #[serde(rename = "CRC64NVME")]
    Crc64Nvme,
    #[serde(rename = "SHA1")]
    Sha1,
    #[serde(rename = "SHA256")]
    Sha256,
}

impl Algorithm {
    pub const ALL: [Algorithm; 5] = [
        Algorithm::Crc32,
        Algorithm::Crc32c,
        Algorithm::Crc64Nvme,
        Algorithm::Sha1,
        Algorithm::Sha256,
    ];

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_uppercase().as_str() {
            "CRC32" => Self::Crc32,
            "CRC32C" => Self::Crc32c,
            "CRC64NVME" => Self::Crc64Nvme,
            "SHA1" => Self::Sha1,
            "SHA256" => Self::Sha256,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Crc32 => "CRC32",
            Self::Crc32c => "CRC32C",
            Self::Crc64Nvme => "CRC64NVME",
            Self::Sha1 => "SHA1",
            Self::Sha256 => "SHA256",
        }
    }

    /// Lowercase header name, e.g. `x-amz-checksum-crc32c`.
    pub fn header_name(self) -> &'static str {
        match self {
            Self::Crc32 => "x-amz-checksum-crc32",
            Self::Crc32c => "x-amz-checksum-crc32c",
            Self::Crc64Nvme => "x-amz-checksum-crc64nvme",
            Self::Sha1 => "x-amz-checksum-sha1",
            Self::Sha256 => "x-amz-checksum-sha256",
        }
    }

    /// XML element name in responses, e.g. `ChecksumCRC32C`.
    pub fn xml_name(self) -> &'static str {
        match self {
            Self::Crc32 => "ChecksumCRC32",
            Self::Crc32c => "ChecksumCRC32C",
            Self::Crc64Nvme => "ChecksumCRC64NVME",
            Self::Sha1 => "ChecksumSHA1",
            Self::Sha256 => "ChecksumSHA256",
        }
    }

    pub fn from_header_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.header_name() == name)
    }

    pub fn digest_len(self) -> usize {
        match self {
            Self::Crc32 | Self::Crc32c => 4,
            Self::Crc64Nvme => 8,
            Self::Sha1 => 20,
            Self::Sha256 => 32,
        }
    }

    pub fn supports(self, kind: ChecksumType) -> bool {
        match kind {
            ChecksumType::FullObject => {
                matches!(self, Self::Crc32 | Self::Crc32c | Self::Crc64Nvme)
            }
            ChecksumType::Composite => !matches!(self, Self::Crc64Nvme),
        }
    }

    /// Default checksum type for a multipart upload with this algorithm.
    pub fn default_multipart_type(self) -> ChecksumType {
        match self {
            Self::Crc64Nvme => ChecksumType::FullObject,
            _ => ChecksumType::Composite,
        }
    }

    pub fn hash(self, data: &[u8]) -> Vec<u8> {
        let mut h = Hasher::new(self);
        h.update(data);
        h.finalize()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ChecksumType {
    #[serde(rename = "FULL_OBJECT")]
    FullObject,
    #[serde(rename = "COMPOSITE")]
    Composite,
}

impl ChecksumType {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_uppercase().as_str() {
            "FULL_OBJECT" => Some(Self::FullObject),
            "COMPOSITE" => Some(Self::Composite),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::FullObject => "FULL_OBJECT",
            Self::Composite => "COMPOSITE",
        }
    }
}

/// A validated S3 checksum in its wire representation (base64, plus `-N` for
/// composite multipart values).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredChecksum {
    pub algorithm: Algorithm,
    #[serde(rename = "type")]
    pub kind: ChecksumType,
    pub value: String,
}

impl StoredChecksum {
    pub fn full(algorithm: Algorithm, digest: &[u8]) -> Self {
        Self {
            algorithm,
            kind: ChecksumType::FullObject,
            value: b64(digest),
        }
    }

    /// Binary digest (without any `-N` suffix).
    pub fn digest(&self) -> Option<Vec<u8>> {
        let base = self.value.split('-').next().unwrap_or("");
        decode_digest(self.algorithm, base)
    }
}

pub fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Decode a base64 checksum and verify its length for the algorithm.
pub fn decode_digest(alg: Algorithm, s: &str) -> Option<Vec<u8>> {
    let v = base64::engine::general_purpose::STANDARD
        .decode(s.trim())
        .ok()?;
    (v.len() == alg.digest_len()).then_some(v)
}

/// Composite multipart checksum: hash of the concatenated binary part
/// checksums, with a part-count suffix.
pub fn composite(alg: Algorithm, parts: &[Vec<u8>]) -> String {
    let mut h = Hasher::new(alg);
    for p in parts {
        h.update(p);
    }
    format!("{}-{}", b64(&h.finalize()), parts.len())
}

/// Multipart ETag: hex(MD5(concat binary part MD5s))-N.
pub fn multipart_etag(part_md5s: &[[u8; 16]]) -> String {
    let mut h = Md5::new();
    for d in part_md5s {
        h.update(d);
    }
    format!("{}-{}", hex::encode(h.finalize()), part_md5s.len())
}

pub enum Hasher {
    Crc32(crc32fast::Hasher),
    Crc32c(u32),
    Crc64(crc::Digest<'static, u64>),
    Sha1(Sha1),
    Sha256(Sha256),
}

impl Hasher {
    pub fn new(alg: Algorithm) -> Self {
        match alg {
            Algorithm::Crc32 => Self::Crc32(crc32fast::Hasher::new()),
            Algorithm::Crc32c => Self::Crc32c(0),
            Algorithm::Crc64Nvme => Self::Crc64(CRC64_NVME.digest()),
            Algorithm::Sha1 => Self::Sha1(Sha1::new()),
            Algorithm::Sha256 => Self::Sha256(Sha256::new()),
        }
    }

    pub fn update(&mut self, data: &[u8]) {
        match self {
            Self::Crc32(h) => h.update(data),
            Self::Crc32c(c) => *c = crc32c::crc32c_append(*c, data),
            Self::Crc64(d) => d.update(data),
            Self::Sha1(h) => h.update(data),
            Self::Sha256(h) => h.update(data),
        }
    }

    pub fn finalize(self) -> Vec<u8> {
        match self {
            Self::Crc32(h) => h.finalize().to_be_bytes().to_vec(),
            Self::Crc32c(c) => c.to_be_bytes().to_vec(),
            Self::Crc64(d) => d.finalize().to_be_bytes().to_vec(),
            Self::Sha1(h) => h.finalize().to_vec(),
            Self::Sha256(h) => h.finalize().to_vec(),
        }
    }
}

/// Hashes computed while a body streams to disk.
pub struct BodyHashes {
    md5: Md5,
    sha256: Sha256,
    extra: Vec<(Algorithm, Hasher)>,
    len: u64,
}

#[derive(Clone, Debug)]
pub struct Digests {
    pub len: u64,
    pub md5: [u8; 16],
    pub sha256: [u8; 32],
    pub checksums: Vec<(Algorithm, Vec<u8>)>,
}

impl Digests {
    pub fn get(&self, alg: Algorithm) -> Option<&[u8]> {
        self.checksums
            .iter()
            .find(|(a, _)| *a == alg)
            .map(|(_, d)| d.as_slice())
    }
}

impl BodyHashes {
    pub fn new(algorithms: &[Algorithm]) -> Self {
        let mut extra: Vec<(Algorithm, Hasher)> = Vec::new();
        for a in algorithms {
            if !extra.iter().any(|(x, _)| x == a) {
                extra.push((*a, Hasher::new(*a)));
            }
        }
        Self {
            md5: Md5::new(),
            sha256: Sha256::new(),
            extra,
            len: 0,
        }
    }

    pub fn update(&mut self, data: &[u8]) {
        self.md5.update(data);
        self.sha256.update(data);
        for (_, h) in &mut self.extra {
            h.update(data);
        }
        self.len += data.len() as u64;
    }

    pub fn finish(self) -> Digests {
        Digests {
            len: self.len,
            md5: self.md5.finalize().into(),
            sha256: self.sha256.finalize().into(),
            checksums: self
                .extra
                .into_iter()
                .map(|(a, h)| (a, h.finalize()))
                .collect(),
        }
    }
}

pub fn md5_b64(data: &[u8]) -> String {
    b64(&Md5::digest(data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_values() {
        let data = b"123456789";
        let hexval = |a: Algorithm| hex::encode(a.hash(data));
        assert_eq!(hexval(Algorithm::Crc32), "cbf43926");
        assert_eq!(hexval(Algorithm::Crc32c), "e3069283");
        assert_eq!(hexval(Algorithm::Crc64Nvme), "ae8b14860a799888");
        assert_eq!(
            hexval(Algorithm::Sha1),
            "f7c3bc1d808e04732adf679965ccc34ca7ae3441"
        );
        assert_eq!(
            hexval(Algorithm::Sha256),
            "15e2b0d3c33891ebb0f1ef609ec419420c20e320ce94c65fbc8c3312448eb225"
        );
    }

    #[test]
    fn incremental_equals_one_shot() {
        let data: Vec<u8> = (0..100_000u32).map(|i| (i * 7 % 251) as u8).collect();
        let mut h = BodyHashes::new(&Algorithm::ALL);
        for chunk in data.chunks(777) {
            h.update(chunk);
        }
        let d = h.finish();
        for a in Algorithm::ALL {
            assert_eq!(d.get(a).unwrap(), a.hash(&data).as_slice(), "{a:?}");
        }
        assert_eq!(d.len, data.len() as u64);
    }

    #[test]
    fn known_s3_representations() {
        // Empty-body values documented for S3 checksums.
        assert_eq!(b64(&Algorithm::Crc32.hash(b"")), "AAAAAA==");
        assert_eq!(b64(&Algorithm::Crc64Nvme.hash(b"")), "AAAAAAAAAAA=");
        assert_eq!(md5_b64(b""), "1B2M2Y8AsgTpgAmY7PhCfg==");
    }

    #[test]
    fn multipart_etag_uses_binary_digests() {
        let a: [u8; 16] = Md5::digest(b"part one").into();
        let b: [u8; 16] = Md5::digest(b"part two").into();
        let mut cat = Vec::new();
        cat.extend_from_slice(&a);
        cat.extend_from_slice(&b);
        let expected = format!("{}-2", hex::encode(Md5::digest(&cat)));
        assert_eq!(multipart_etag(&[a, b]), expected);
    }

    #[test]
    fn decode_rejects_wrong_lengths() {
        assert!(decode_digest(Algorithm::Crc32, "AAAAAA==").is_some());
        assert!(decode_digest(Algorithm::Crc32, "AAAAAAAAAAA=").is_none());
        assert!(decode_digest(Algorithm::Sha256, "not base64!").is_none());
    }

    #[test]
    fn type_support_matrix() {
        use ChecksumType::*;
        assert!(Algorithm::Crc64Nvme.supports(FullObject));
        assert!(!Algorithm::Crc64Nvme.supports(Composite));
        assert!(!Algorithm::Sha256.supports(FullObject));
        assert!(Algorithm::Crc32.supports(FullObject) && Algorithm::Crc32.supports(Composite));
    }
}
