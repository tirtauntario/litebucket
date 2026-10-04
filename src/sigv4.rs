//! AWS Signature Version 4 primitives for S3 (header, query, chunk, trailer).
//!
//! Canonicalization follows the S3 rules: paths are not normalized, each path
//! byte is URI-encoded once, query parameters are decoded then re-encoded and
//! sorted. Signatures are compared in constant time.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

pub const ALGORITHM: &str = "AWS4-HMAC-SHA256";
pub const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

type HmacSha256 = Hmac<Sha256>;

pub fn hmac(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut m = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    m.update(msg);
    m.finalize().into_bytes().into()
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

pub fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> [u8; 32] {
    let k_date = hmac(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k_region = hmac(&k_date, region.as_bytes());
    let k_service = hmac(&k_region, service.as_bytes());
    hmac(&k_service, b"aws4_request")
}

/// Constant-time comparison of a computed signature with a client hex string.
pub fn signature_eq(computed: &[u8; 32], provided_hex: &str) -> bool {
    let Ok(provided) = hex::decode(provided_hex) else {
        return false;
    };
    provided.len() == 32 && bool::from(computed.as_slice().ct_eq(&provided))
}

fn is_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~')
}

/// AWS UriEncode: unreserved bytes verbatim, others as uppercase `%XX`.
pub fn uri_encode(input: &[u8], encode_slash: bool) -> String {
    let mut out = String::with_capacity(input.len() * 3);
    for &b in input {
        if is_unreserved(b) || (b == b'/' && !encode_slash) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Percent-decode a URI component. `+` is literal. Malformed escapes are kept
/// verbatim, matching lenient server behavior without inventing bytes.
pub fn percent_decode(s: &str) -> Vec<u8> {
    percent_encoding::percent_decode_str(s).collect()
}

/// Canonical URI candidates: the decode-then-encode form used by AWS SDKs,
/// and the raw path as sent (for clients that encode extra characters).
pub fn canonical_uri_candidates(raw_path: &str) -> Vec<String> {
    let path = if raw_path.is_empty() { "/" } else { raw_path };
    let normalized = uri_encode(&percent_decode(path), false);
    if normalized == path {
        vec![normalized]
    } else {
        vec![normalized, path.to_string()]
    }
}

/// Canonical query string. `skip` names parameters excluded from signing
/// (`X-Amz-Signature` for presigned URLs).
pub fn canonical_query(raw_query: &str, skip: Option<&str>) -> String {
    let mut pairs: Vec<(String, String)> = raw_query
        .split('&')
        .filter(|s| !s.is_empty())
        .filter_map(|part| {
            let (k, v) = part.split_once('=').unwrap_or((part, ""));
            let k = percent_decode(k);
            if skip.is_some_and(|s| s.as_bytes() == k.as_slice()) {
                return None;
            }
            Some((uri_encode(&k, true), uri_encode(&percent_decode(v), true)))
        })
        .collect();
    pairs.sort();
    pairs
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// Trim and collapse runs of spaces, as SigV4 requires for header values.
pub fn canonical_header_value(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut space = false;
    for c in v.trim().chars() {
        if c == ' ' || c == '\t' {
            if !space {
                out.push(' ');
            }
            space = true;
        } else {
            out.push(c);
            space = false;
        }
    }
    out
}

/// Canonical headers block for the signed header names, in the given order.
/// Returns None if a signed header is absent from the request.
pub fn canonical_headers(headers: &http::HeaderMap, signed: &[String]) -> Option<String> {
    let mut out = String::new();
    for name in signed {
        let values: Vec<String> = headers
            .get_all(name.as_str())
            .iter()
            .map(|v| canonical_header_value(&String::from_utf8_lossy(v.as_bytes())))
            .collect();
        if values.is_empty() {
            return None;
        }
        out.push_str(name);
        out.push(':');
        out.push_str(&values.join(","));
        out.push('\n');
    }
    Some(out)
}

pub fn canonical_request(
    method: &str,
    uri: &str,
    query: &str,
    headers_block: &str,
    signed_headers: &str,
    payload_hash: &str,
) -> String {
    format!("{method}\n{uri}\n{query}\n{headers_block}\n{signed_headers}\n{payload_hash}")
}

pub fn string_to_sign(amz_date: &str, scope: &str, canonical_request: &str) -> String {
    format!(
        "{ALGORITHM}\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    )
}

pub fn chunk_string_to_sign(
    amz_date: &str,
    scope: &str,
    prev_sig: &str,
    chunk_sha256_hex: &str,
) -> String {
    format!(
        "AWS4-HMAC-SHA256-PAYLOAD\n{amz_date}\n{scope}\n{prev_sig}\n{EMPTY_SHA256}\n{chunk_sha256_hex}"
    )
}

pub fn trailer_string_to_sign(
    amz_date: &str,
    scope: &str,
    prev_sig: &str,
    trailer_sha256_hex: &str,
) -> String {
    format!("AWS4-HMAC-SHA256-TRAILER\n{amz_date}\n{scope}\n{prev_sig}\n{trailer_sha256_hex}")
}

/// Parsed `Credential=AKID/20130524/us-east-1/s3/aws4_request`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialScope {
    pub access_key: String,
    pub date: String,
    pub region: String,
    pub service: String,
}

impl CredentialScope {
    pub fn parse(s: &str) -> Option<Self> {
        let mut it = s.split('/');
        let access_key = it.next()?.to_string();
        let date = it.next()?.to_string();
        let region = it.next()?.to_string();
        let service = it.next()?.to_string();
        let term = it.next()?;
        if it.next().is_some() || term != "aws4_request" || access_key.is_empty() {
            return None;
        }
        if date.len() != 8 || !date.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        Some(Self {
            access_key,
            date,
            region,
            service,
        })
    }

    pub fn scope(&self) -> String {
        format!(
            "{}/{}/{}/aws4_request",
            self.date, self.region, self.service
        )
    }
}

/// Parsed header-based authorization.
#[derive(Debug, Clone)]
pub struct AuthorizationHeader {
    pub credential: CredentialScope,
    pub signed_headers: Vec<String>,
    pub signature: String,
}

impl AuthorizationHeader {
    /// Parse `AWS4-HMAC-SHA256 Credential=..., SignedHeaders=..., Signature=...`.
    pub fn parse(value: &str) -> Result<Self, &'static str> {
        let rest = value
            .strip_prefix(ALGORITHM)
            .ok_or("unsupported authorization algorithm")?;
        if !rest.starts_with(' ') {
            return Err("unsupported authorization algorithm");
        }
        let (mut cred, mut signed, mut sig) = (None, None, None);
        for part in rest.split(',') {
            let part = part.trim();
            let (k, v) = part
                .split_once('=')
                .ok_or("malformed authorization header")?;
            match k {
                "Credential" => cred = Some(v),
                "SignedHeaders" => signed = Some(v),
                "Signature" => sig = Some(v),
                _ => return Err("malformed authorization header"),
            }
        }
        let credential = CredentialScope::parse(cred.ok_or("missing Credential")?)
            .ok_or("malformed Credential")?;
        let signed_headers = parse_signed_headers(signed.ok_or("missing SignedHeaders")?)?;
        let signature = sig.ok_or("missing Signature")?.to_string();
        Ok(Self {
            credential,
            signed_headers,
            signature,
        })
    }
}

pub fn parse_signed_headers(s: &str) -> Result<Vec<String>, &'static str> {
    let v: Vec<String> = s.split(';').map(|h| h.to_string()).collect();
    if v.iter().any(|h| {
        h.is_empty()
            || h.bytes()
                .any(|b| b.is_ascii_uppercase() || b.is_ascii_whitespace())
    }) {
        return Err("malformed SignedHeaders");
    }
    if !v.windows(2).all(|w| w[0] < w[1]) {
        return Err("SignedHeaders must be sorted and unique");
    }
    Ok(v)
}

/// Parse `YYYYMMDDTHHMMSSZ` into Unix seconds.
pub fn parse_amz_date(s: &str) -> Option<i64> {
    let fmt = time::macros::format_description!("[year][month][day]T[hour][minute][second]Z");
    time::PrimitiveDateTime::parse(s, &fmt)
        .ok()
        .map(|t| t.assume_utc().unix_timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
    const DATE: &str = "20130524T000000Z";

    fn headers(pairs: &[(&str, &str)]) -> http::HeaderMap {
        let mut h = http::HeaderMap::new();
        for (k, v) in pairs {
            h.append(
                http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                http::HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    fn sign(
        method: &str,
        path: &str,
        query: &str,
        h: &http::HeaderMap,
        signed: &str,
        payload: &str,
    ) -> String {
        let signed_v: Vec<String> = signed.split(';').map(String::from).collect();
        let block = canonical_headers(h, &signed_v).unwrap();
        let uri = &canonical_uri_candidates(path)[0];
        let cr = canonical_request(
            method,
            uri,
            &canonical_query(query, Some("X-Amz-Signature")),
            &block,
            signed,
            payload,
        );
        let sts = string_to_sign(DATE, "20130524/us-east-1/s3/aws4_request", &cr);
        hex::encode(hmac(
            &signing_key(SECRET, "20130524", "us-east-1", "s3"),
            sts.as_bytes(),
        ))
    }

    // Vectors from the Amazon S3 "Signature Calculations for the Authorization
    // Header" and "Query String" documentation.

    #[test]
    fn aws_get_object_vector() {
        let h = headers(&[
            ("host", "examplebucket.s3.amazonaws.com"),
            ("range", "bytes=0-9"),
            ("x-amz-content-sha256", EMPTY_SHA256),
            ("x-amz-date", DATE),
        ]);
        assert_eq!(
            sign(
                "GET",
                "/test.txt",
                "",
                &h,
                "host;range;x-amz-content-sha256;x-amz-date",
                EMPTY_SHA256
            ),
            "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    #[test]
    fn aws_put_object_vector() {
        let payload = sha256_hex(b"Welcome to Amazon S3.");
        assert_eq!(
            payload,
            "44ce7dd67c959e0d3524ffac1771dfbba87d2b6b4b4e99e42034a8b803f8b072"
        );
        let h = headers(&[
            ("date", "Fri, 24 May 2013 00:00:00 GMT"),
            ("host", "examplebucket.s3.amazonaws.com"),
            ("x-amz-content-sha256", &payload),
            ("x-amz-date", DATE),
            ("x-amz-storage-class", "REDUCED_REDUNDANCY"),
        ]);
        assert_eq!(
            sign(
                "PUT",
                "/test$file.text",
                "",
                &h,
                "date;host;x-amz-content-sha256;x-amz-date;x-amz-storage-class",
                &payload
            ),
            "98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd"
        );
    }

    #[test]
    fn aws_query_parameter_vectors() {
        let h = headers(&[
            ("host", "examplebucket.s3.amazonaws.com"),
            ("x-amz-content-sha256", EMPTY_SHA256),
            ("x-amz-date", DATE),
        ]);
        let signed = "host;x-amz-content-sha256;x-amz-date";
        assert_eq!(
            sign("GET", "/", "lifecycle", &h, signed, EMPTY_SHA256),
            "fea454ca298b7da1c68078a5d1bdbfbbe0d65c699e0f91ac7a200a0136783543"
        );
        assert_eq!(
            sign("GET", "/", "max-keys=2&prefix=J", &h, signed, EMPTY_SHA256),
            "34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"
        );
    }

    #[test]
    fn aws_presigned_url_vector() {
        let h = headers(&[("host", "examplebucket.s3.amazonaws.com")]);
        let q = "X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=AKIAIOSFODNN7EXAMPLE%2F20130524%2Fus-east-1%2Fs3%2Faws4_request&X-Amz-Date=20130524T000000Z&X-Amz-Expires=86400&X-Amz-SignedHeaders=host&X-Amz-Signature=ignored";
        assert_eq!(
            sign("GET", "/test.txt", q, &h, "host", "UNSIGNED-PAYLOAD"),
            "aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404"
        );
    }

    #[test]
    fn aws_streaming_chunk_vectors() {
        let h = headers(&[
            ("content-encoding", "aws-chunked"),
            ("content-length", "66824"),
            ("host", "s3.amazonaws.com"),
            ("x-amz-content-sha256", "STREAMING-AWS4-HMAC-SHA256-PAYLOAD"),
            ("x-amz-date", DATE),
            ("x-amz-decoded-content-length", "66560"),
            ("x-amz-storage-class", "REDUCED_REDUNDANCY"),
        ]);
        let seed = sign(
            "PUT",
            "/examplebucket/chunkObject.txt",
            "",
            &h,
            "content-encoding;content-length;host;x-amz-content-sha256;x-amz-date;x-amz-decoded-content-length;x-amz-storage-class",
            "STREAMING-AWS4-HMAC-SHA256-PAYLOAD",
        );
        assert_eq!(
            seed,
            "4f232c4386841ef735655705268965c44a0e4690baa4adea153f7db9fa80a0a9"
        );
        let key = signing_key(SECRET, "20130524", "us-east-1", "s3");
        let scope = "20130524/us-east-1/s3/aws4_request";
        let chunk = |prev: &str, data: &[u8]| {
            hex::encode(hmac(
                &key,
                chunk_string_to_sign(DATE, scope, prev, &sha256_hex(data)).as_bytes(),
            ))
        };
        let c1 = chunk(&seed, &vec![b'a'; 65536]);
        assert_eq!(
            c1,
            "ad80c730a21e5b8d04586a2213dd63b9a0e99e0e2307b0ade35a65485a288648"
        );
        let c2 = chunk(&c1, &vec![b'a'; 1024]);
        assert_eq!(
            c2,
            "0055627c9e194cb4542bae2aa5492e3c1575bbb81b612b7d234b86a503ef5497"
        );
        let c3 = chunk(&c2, b"");
        assert_eq!(
            c3,
            "b6c6ea8a5354eaf15b3cb7646744f4275b71ea724fed81ceb9323e279d449df9"
        );
    }

    #[test]
    fn aws_streaming_trailer_vectors() {
        // "Signature calculation: transfer payload in multiple chunks with trailer".
        let key = signing_key(SECRET, "20130524", "us-east-1", "s3");
        let scope = "20130524/us-east-1/s3/aws4_request";
        let chunk = |prev: &str, data: &[u8]| {
            hex::encode(hmac(
                &key,
                chunk_string_to_sign(DATE, scope, prev, &sha256_hex(data)).as_bytes(),
            ))
        };
        let c1 = chunk(
            "106e2a8a18243abcf37539882f36619c00e2dfc72633413f02d3b74544bfeb8e",
            &vec![b'a'; 65536],
        );
        assert_eq!(
            c1,
            "b474d8862b1487a5145d686f57f013e54db672cee1c953b3010fb58501ef5aa2"
        );
        let c2 = chunk(&c1, &vec![b'a'; 1024]);
        assert_eq!(
            c2,
            "1c1344b170168f8e65b41376b44b20fe354e373826ccbbe2c1d40a8cae51e5c7"
        );
        let c3 = chunk(&c2, b"");
        assert_eq!(
            c3,
            "2ca2aba2005185cf7159c6277faf83795951dd77a3a99e6e65d5c9f85863f992"
        );
        let trailer = sha256_hex(b"x-amz-checksum-crc32c:sOO8/Q==\n");
        let t = hex::encode(hmac(
            &key,
            trailer_string_to_sign(DATE, scope, &c3, &trailer).as_bytes(),
        ));
        assert_eq!(
            t,
            "d81f82fc3505edab99d459891051a732e8730629a2e4a59689829ca17fe2e435"
        );
    }

    #[test]
    fn uri_and_query_canonicalization() {
        assert_eq!(
            canonical_uri_candidates("/a%20b/c+d/~x")[0],
            "/a%20b/c%2Bd/~x"
        );
        assert_eq!(canonical_uri_candidates("//a/./../b/")[0], "//a/./../b/");
        assert_eq!(canonical_query("b=2&a=1&a=0&c", None), "a=0&a=1&b=2&c=");
        assert_eq!(
            canonical_query("prefix=a%2Fb%20c", None),
            "prefix=a%2Fb%20c"
        );
        assert_eq!(canonical_header_value("  a   b  c "), "a b c");
    }

    #[test]
    fn authorization_parsing() {
        let a = AuthorizationHeader::parse(
            "AWS4-HMAC-SHA256 Credential=AKID/20130524/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-date, Signature=abc",
        )
        .unwrap();
        assert_eq!(a.credential.access_key, "AKID");
        assert_eq!(a.signed_headers, vec!["host", "x-amz-date"]);
        assert!(AuthorizationHeader::parse("AWS AKID:sig").is_err());
        assert!(AuthorizationHeader::parse("AWS4-ECDSA-P256-SHA256 Credential=x").is_err());
        assert!(
            AuthorizationHeader::parse(
                "AWS4-HMAC-SHA256 Credential=AKID/20130524/us-east-1/s3/aws4_request, SignedHeaders=x-amz-date;host, Signature=abc"
            )
            .is_err()
        );
        assert_eq!(parse_amz_date("20130524T000000Z"), Some(1_369_353_600));
        assert!(parse_amz_date("2013-05-24").is_none());
    }

    #[test]
    fn constant_time_signature_compare() {
        let s = [7u8; 32];
        assert!(signature_eq(&s, &hex::encode(s)));
        assert!(!signature_eq(&s, &hex::encode([8u8; 32])));
        assert!(!signature_eq(&s, "zz"));
    }
}
