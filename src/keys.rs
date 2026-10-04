//! Validated bucket names and object keys.

use std::fmt;

pub const MAX_KEY_BYTES: usize = 1024;

/// Characters permitted by XML 1.0. Keys outside this set are rejected so every
/// listing can be serialized as valid XML.
pub fn is_xml_char(c: char) -> bool {
    matches!(c,
        '\u{9}' | '\u{A}' | '\u{D}'
        | '\u{20}'..='\u{D7FF}'
        | '\u{E000}'..='\u{FFFD}'
        | '\u{10000}'..='\u{10FFFF}')
}

/// An object key: exact UTF-8 bytes, 1..=1024 bytes, XML-safe code points only.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectKey(String);

impl ObjectKey {
    pub fn parse(s: String) -> Result<Self, &'static str> {
        if s.is_empty() {
            return Err("object key must not be empty");
        }
        if s.len() > MAX_KEY_BYTES {
            return Err("object key exceeds 1024 bytes");
        }
        if !s.chars().all(is_xml_char) {
            return Err("object key contains a character that is not supported by this service");
        }
        Ok(Self(s))
    }

    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, &'static str> {
        let s = String::from_utf8(bytes).map_err(|_| "object key is not valid UTF-8")?;
        Self::parse(s)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl fmt::Debug for ObjectKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Keys may be sensitive; only their length is shown in debug output.
        write!(f, "ObjectKey(len={})", self.0.len())
    }
}

/// A general-purpose bucket name validated against the S3 naming rules.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct BucketName(String);

impl BucketName {
    pub fn parse(s: &str) -> Result<Self, &'static str> {
        validate_bucket_name(s)?;
        Ok(Self(s.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BucketName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

pub fn validate_bucket_name(s: &str) -> Result<(), &'static str> {
    let b = s.as_bytes();
    if !(3..=63).contains(&b.len()) {
        return Err("bucket names must be between 3 and 63 characters long");
    }
    if !b
        .iter()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'.' || *c == b'-')
    {
        return Err(
            "bucket names can consist only of lowercase letters, numbers, dots, and hyphens",
        );
    }
    let alnum = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    if !alnum(b[0]) || !alnum(b[b.len() - 1]) {
        return Err("bucket names must begin and end with a letter or number");
    }
    if s.contains("..") {
        return Err("bucket names must not contain two adjacent periods");
    }
    if s.parse::<std::net::Ipv4Addr>().is_ok() {
        return Err("bucket names must not be formatted as an IP address");
    }
    for prefix in ["xn--", "sthree-", "amzn-s3-demo-"] {
        if s.starts_with(prefix) {
            return Err("bucket name uses a reserved prefix");
        }
    }
    for suffix in ["-s3alias", "--ol-s3", ".mrap", "--x-s3", "--table-s3"] {
        if s.ends_with(suffix) {
            return Err("bucket name uses a reserved suffix");
        }
    }
    Ok(())
}

/// Smallest byte string greater than every string with prefix `p`, if any.
pub fn prefix_successor(p: &[u8]) -> Option<Vec<u8>> {
    let mut v = p.to_vec();
    while let Some(last) = v.pop() {
        if last < 0xFF {
            v.push(last + 1);
            return Some(v);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_names() {
        for ok in ["abc", "my-bucket.1", "a".repeat(63).as_str(), "1bucket"] {
            assert!(validate_bucket_name(ok).is_ok(), "{ok}");
        }
        for bad in [
            "ab",
            &"a".repeat(64),
            "My-Bucket",
            "-abc",
            "abc-",
            "a..b",
            "192.168.1.1",
            "xn--abc",
            "sthree-x",
            "abc-s3alias",
            "abc--ol-s3",
            "under_score",
            "abc/def",
        ] {
            assert!(validate_bucket_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn object_keys_preserve_exact_bytes() {
        let k = ObjectKey::parse("a//b/./../é\u{0301}\t\n\r +%".into()).unwrap();
        assert_eq!(k.as_str(), "a//b/./../é\u{0301}\t\n\r +%");
        assert!(ObjectKey::parse(String::new()).is_err());
        assert!(ObjectKey::parse("a\u{0}b".into()).is_err());
        assert!(ObjectKey::parse("a\u{1}b".into()).is_err());
        assert!(ObjectKey::parse("a\u{FFFE}".into()).is_err());
        assert!(ObjectKey::parse("k".repeat(1024)).is_ok());
        assert!(ObjectKey::parse("k".repeat(1025)).is_err());
        assert!(ObjectKey::from_bytes(vec![0xff, 0xfe]).is_err());
    }

    #[test]
    fn successor() {
        assert_eq!(prefix_successor(b"abc"), Some(b"abd".to_vec()));
        assert_eq!(prefix_successor(b"ab\xff"), Some(b"ac".to_vec()));
        assert_eq!(prefix_successor(b"\xff\xff"), None);
        assert_eq!(prefix_successor(b""), None);
    }
}
