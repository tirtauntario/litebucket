//! Parsed request context: raw URI preserved for signing, path-style bucket
//! and key extraction, decoded query parameters, and header helpers.

use http::{HeaderMap, Method};

use super::error::{S3Error, S3Result};
use crate::keys::ObjectKey;
use crate::sigv4::percent_decode;

#[derive(Debug)]
pub struct S3Request {
    pub id: String,
    pub method: Method,
    /// Path exactly as received (still percent-encoded).
    pub raw_path: String,
    /// Query exactly as received (without `?`).
    pub raw_query: String,
    /// Decoded query parameters in request order.
    pub query: Vec<(String, String)>,
    pub headers: HeaderMap,
    pub bucket: Option<String>,
    pub key: Option<ObjectKey>,
}

impl S3Request {
    pub fn parse(id: String, method: Method, uri: &http::Uri, headers: HeaderMap) -> S3Result<Self> {
        let raw_path = uri.path().to_string();
        let raw_query = uri.query().unwrap_or("").to_string();
        let mut query = Vec::new();
        for part in raw_query.split('&').filter(|s| !s.is_empty()) {
            let (k, v) = part.split_once('=').unwrap_or((part, ""));
            let k = String::from_utf8(percent_decode(k))
                .map_err(|_| S3Error::invalid_argument("query parameter name is not valid UTF-8"))?;
            let v = String::from_utf8(percent_decode(v))
                .map_err(|_| S3Error::invalid_argument("query parameter value is not valid UTF-8"))?;
            query.push((k, v));
        }
        let (bucket, key) = split_path(&raw_path)?;
        Ok(Self {
            id,
            method,
            raw_path,
            raw_query,
            query,
            headers,
            bucket,
            key,
        })
    }

    pub fn q(&self, name: &str) -> Option<&str> {
        self.query.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    pub fn has_q(&self, name: &str) -> bool {
        self.query.iter().any(|(k, _)| k == name)
    }

    /// A single-valued header as UTF-8. Duplicates are rejected.
    pub fn header(&self, name: &str) -> S3Result<Option<&str>> {
        let mut it = self.headers.get_all(name).iter();
        let Some(v) = it.next() else { return Ok(None) };
        if it.next().is_some() {
            return Err(S3Error::invalid_argument(format!("header {name} must not be repeated")));
        }
        v.to_str()
            .map(Some)
            .map_err(|_| S3Error::invalid_argument(format!("header {name} is not valid ASCII")))
    }

    pub fn header_or_empty(&self, name: &str) -> &str {
        self.headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or("")
    }

    pub fn bucket_name(&self) -> &str {
        self.bucket.as_deref().unwrap_or("")
    }

    pub fn object_key(&self) -> &ObjectKey {
        self.key.as_ref().expect("object operation has a key")
    }

    pub fn content_length(&self) -> S3Result<Option<u64>> {
        match self.header("content-length")? {
            None => Ok(None),
            Some(v) => v
                .parse::<u64>()
                .map(Some)
                .map_err(|_| S3Error::invalid_argument("invalid Content-Length")),
        }
    }
}

/// Path-style split: `/bucket` or `/bucket/` → bucket level; `/bucket/key...`
/// → object level with the key decoded exactly once.
pub fn split_path(raw_path: &str) -> S3Result<(Option<String>, Option<ObjectKey>)> {
    let Some(rest) = raw_path.strip_prefix('/') else {
        return Err(S3Error::invalid_request("request path must be absolute"));
    };
    if rest.is_empty() {
        return Ok((None, None));
    }
    let (bucket_raw, key_raw) = match rest.find('/') {
        Some(i) => (&rest[..i], Some(&rest[i + 1..])),
        None => (rest, None),
    };
    let bucket = String::from_utf8(percent_decode(bucket_raw))
        .map_err(|_| S3Error::invalid_bucket_name("bucket name is not valid UTF-8"))?;
    if bucket.is_empty() {
        return Err(S3Error::invalid_bucket_name("empty bucket name"));
    }
    let key = match key_raw {
        None | Some("") => None,
        Some(k) => Some(
            ObjectKey::from_bytes(percent_decode(k)).map_err(S3Error::invalid_argument)?,
        ),
    };
    Ok((Some(bucket), key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_style_split_preserves_key_bytes() {
        let (b, k) = split_path("/docs/a//b/./../c%20d+e%2Fx").unwrap();
        assert_eq!(b.as_deref(), Some("docs"));
        assert_eq!(k.unwrap().as_str(), "a//b/./../c d+e/x");
        let (b, k) = split_path("/docs/").unwrap();
        assert_eq!((b.as_deref(), k.is_none()), (Some("docs"), true));
        let (b, k) = split_path("/").unwrap();
        assert!(b.is_none() && k.is_none());
        let (_, k) = split_path("/docs/dir/").unwrap();
        assert_eq!(k.unwrap().as_str(), "dir/");
        assert!(split_path("/docs/%00").is_err());
        assert!(split_path("//x").is_err());
    }
}
