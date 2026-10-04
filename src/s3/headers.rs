//! Header parsing (content headers, user metadata, conditions, ranges) and
//! response formatting helpers.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use http::{HeaderValue, Response, StatusCode};

use super::error::{S3Error, S3Result};
use super::request::S3Request;
use crate::metadata::queries::{ContentHeaders, UserMetadata, WriteConditions};

const MAX_CONTENT_HEADER_BYTES: usize = 8 * 1024;

fn header_text(req: &S3Request, name: &str) -> S3Result<Option<String>> {
    let mut it = req.headers.get_all(name).iter();
    let Some(v) = it.next() else { return Ok(None) };
    if it.next().is_some() {
        return Err(S3Error::invalid_argument(format!("header {name} must not be repeated")));
    }
    let s = String::from_utf8(v.as_bytes().to_vec())
        .map_err(|_| S3Error::invalid_argument(format!("header {name} is not valid UTF-8")))?;
    if s.len() > MAX_CONTENT_HEADER_BYTES {
        return Err(S3Error::invalid_argument(format!("header {name} is too long")));
    }
    Ok(Some(s))
}

/// Content headers to persist. Transport-only `aws-chunked` coding is removed.
pub fn content_headers(req: &S3Request) -> S3Result<ContentHeaders> {
    let encoding = header_text(req, "content-encoding")?.and_then(|v| {
        let kept: Vec<&str> = v
            .split(',')
            .map(str::trim)
            .filter(|t| !t.is_empty() && !t.eq_ignore_ascii_case("aws-chunked"))
            .collect();
        (!kept.is_empty()).then(|| kept.join(", "))
    });
    Ok(ContentHeaders {
        content_type: header_text(req, "content-type")?,
        content_disposition: header_text(req, "content-disposition")?,
        content_encoding: encoding,
        content_language: header_text(req, "content-language")?,
        cache_control: header_text(req, "cache-control")?,
        expires: header_text(req, "expires")?,
    })
}

/// `x-amz-meta-*` with lowercase names, bounded in total size.
pub fn user_metadata(req: &S3Request, limit: usize) -> S3Result<UserMetadata> {
    let mut out = UserMetadata::new();
    let mut size = 0usize;
    for (name, value) in req.headers.iter() {
        let Some(suffix) = name.as_str().strip_prefix("x-amz-meta-") else {
            continue;
        };
        if suffix.is_empty() {
            return Err(S3Error::invalid_argument("empty user metadata name"));
        }
        let v = String::from_utf8(value.as_bytes().to_vec())
            .map_err(|_| S3Error::invalid_argument("user metadata values must be valid UTF-8"))?;
        size += suffix.len() + v.len();
        match out.get_mut(suffix) {
            // Repeated metadata headers are combined, as HTTP permits.
            Some(existing) => {
                existing.push(',');
                existing.push_str(&v);
            }
            None => {
                out.insert(suffix.to_string(), v);
            }
        }
    }
    if size > limit {
        return Err(S3Error::new(
            "MetadataTooLarge",
            StatusCode::BAD_REQUEST,
            "Your metadata headers exceed the maximum allowed metadata size",
        ));
    }
    Ok(out)
}

/// Parse an ETag list (`"a", "b"`, `*`, `W/"c"`) into unquoted values.
pub fn etag_list(v: &str) -> Vec<String> {
    v.split(',')
        .map(|s| {
            let s = s.trim();
            let s = s.strip_prefix("W/").unwrap_or(s);
            s.trim_matches('"').to_string()
        })
        .filter(|s| !s.is_empty())
        .collect()
}

pub fn write_conditions(req: &S3Request) -> S3Result<WriteConditions> {
    Ok(WriteConditions {
        if_match: req.header("if-match")?.map(etag_list),
        if_none_match_any: req.header("if-none-match")?.is_some_and(|v| v.trim() == "*"),
    })
}

#[derive(Debug, Default, Clone)]
pub struct ReadConditions {
    pub if_match: Option<Vec<String>>,
    pub if_none_match: Option<Vec<String>>,
    pub if_modified_since: Option<SystemTime>,
    pub if_unmodified_since: Option<SystemTime>,
}

fn http_date_header(req: &S3Request, name: &str) -> S3Result<Option<SystemTime>> {
    // Unparseable dates are ignored, as HTTP requires.
    Ok(req.header(name)?.and_then(|v| httpdate::parse_http_date(v.trim()).ok()))
}

pub fn read_conditions(req: &S3Request, prefix: &str) -> S3Result<ReadConditions> {
    Ok(ReadConditions {
        if_match: req.header(&format!("{prefix}if-match"))?.map(etag_list),
        if_none_match: req.header(&format!("{prefix}if-none-match"))?.map(etag_list),
        if_modified_since: http_date_header(req, &format!("{prefix}if-modified-since"))?,
        if_unmodified_since: http_date_header(req, &format!("{prefix}if-unmodified-since"))?,
    })
}

#[derive(Debug, PartialEq, Eq)]
pub enum CondResult {
    Proceed,
    NotModified,
    PreconditionFailed,
}

/// S3/RFC 9110 precedence: If-Match, else If-Unmodified-Since; then
/// If-None-Match, else If-Modified-Since. Times compare at 1 s resolution.
pub fn evaluate(c: &ReadConditions, etag: &str, last_modified_ms: i64) -> CondResult {
    let lm_secs = last_modified_ms.div_euclid(1000);
    let secs = |t: SystemTime| {
        t.duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    };
    let matches = |list: &Vec<String>| list.iter().any(|e| e == "*" || e == etag);
    if let Some(list) = &c.if_match {
        if !matches(list) {
            return CondResult::PreconditionFailed;
        }
    } else if let Some(t) = c.if_unmodified_since
        && lm_secs > secs(t)
    {
        return CondResult::PreconditionFailed;
    }
    if let Some(list) = &c.if_none_match {
        if matches(list) {
            return CondResult::NotModified;
        }
    } else if let Some(t) = c.if_modified_since
        && lm_secs <= secs(t)
    {
        return CondResult::NotModified;
    }
    CondResult::Proceed
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum RangeSpec {
    FromTo(u64, u64),
    From(u64),
    Suffix(u64),
}

/// Parse a `Range` header. Malformed values are ignored (full response);
/// multiple ranges are rejected explicitly.
pub fn parse_range(v: &str) -> S3Result<Option<RangeSpec>> {
    let Some(spec) = v.trim().strip_prefix("bytes=") else {
        return Ok(None);
    };
    if spec.contains(',') {
        return Err(S3Error::invalid_argument("Multiple byte ranges are not supported"));
    }
    let Some((a, b)) = spec.trim().split_once('-') else {
        return Ok(None);
    };
    let num = |s: &str| -> Option<u64> {
        (!s.is_empty() && s.bytes().all(|c| c.is_ascii_digit()))
            .then(|| s.parse().ok())
            .flatten()
    };
    Ok(match (a.trim(), b.trim()) {
        ("", n) => num(n).map(RangeSpec::Suffix),
        (s, "") => num(s).map(RangeSpec::From),
        (s, e) => match (num(s), num(e)) {
            (Some(s), Some(e)) if s <= e => Some(RangeSpec::FromTo(s, e)),
            _ => None,
        },
    })
}

/// Resolve a range against a size: Ok(Some((start, len))) or unsatisfiable.
pub fn resolve_range(r: RangeSpec, size: u64) -> S3Result<(u64, u64)> {
    let unsat = || S3Error::invalid_range(size);
    match r {
        RangeSpec::FromTo(s, e) => {
            if s >= size {
                return Err(unsat());
            }
            let end = e.min(size - 1);
            Ok((s, end - s + 1))
        }
        RangeSpec::From(s) => {
            if s >= size {
                return Err(unsat());
            }
            Ok((s, size - s))
        }
        RangeSpec::Suffix(n) => {
            if n == 0 || size == 0 {
                return Err(unsat());
            }
            let n = n.min(size);
            Ok((size - n, n))
        }
    }
}

pub fn system_time(ms: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(ms.max(0) as u64)
}

pub fn http_date(ms: i64) -> String {
    httpdate::fmt_http_date(system_time(ms))
}

/// `2009-10-12T17:50:30.000Z`
pub fn iso8601(ms: i64) -> String {
    let t = time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    let fmt = time::macros::format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
    );
    t.format(&fmt).unwrap_or_default()
}

pub fn quote_etag(etag: &str) -> String {
    format!("\"{etag}\"")
}

/// Build a response; header values that fail validation are dropped rather
/// than allowing header injection.
pub fn response<K: AsRef<str>>(status: StatusCode, headers: Vec<(K, String)>, body: Body) -> Response<Body> {
    let mut r = Response::new(body);
    *r.status_mut() = status;
    for (k, v) in headers {
        let k = k.as_ref();
        if let (Ok(name), Ok(value)) = (http::HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(&v)) {
            r.headers_mut().append(name, value);
        } else if let (Ok(name), Ok(value)) = (
            http::HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_bytes(v.as_bytes()),
        ) {
            // Non-ASCII but CR/LF-free UTF-8 (e.g. stored metadata values).
            r.headers_mut().append(name, value);
        }
    }
    r
}

pub fn xml(status: StatusCode, body: String) -> Response<Body> {
    response(status, vec![("content-type", "application/xml".to_string())], Body::from(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-9").unwrap(), Some(RangeSpec::FromTo(0, 9)));
        assert_eq!(parse_range("bytes=5-").unwrap(), Some(RangeSpec::From(5)));
        assert_eq!(parse_range("bytes=-3").unwrap(), Some(RangeSpec::Suffix(3)));
        assert_eq!(parse_range("bytes=9-0").unwrap(), None);
        assert_eq!(parse_range("items=0-1").unwrap(), None);
        assert_eq!(parse_range("bytes=99999999999999999999999-").unwrap(), None);
        assert!(parse_range("bytes=0-1,3-4").is_err());
        assert_eq!(resolve_range(RangeSpec::FromTo(0, 9), 5).unwrap(), (0, 5));
        assert_eq!(resolve_range(RangeSpec::Suffix(3), 10).unwrap(), (7, 3));
        assert_eq!(resolve_range(RangeSpec::Suffix(30), 10).unwrap(), (0, 10));
        assert!(resolve_range(RangeSpec::From(10), 10).is_err());
        assert!(resolve_range(RangeSpec::Suffix(0), 10).is_err());
        assert!(resolve_range(RangeSpec::From(0), 0).is_err());
    }

    #[test]
    fn condition_precedence() {
        let lm = 1_000_000_000_500i64;
        let t = |s: i64| UNIX_EPOCH + Duration::from_secs(s as u64);
        let c = ReadConditions {
            if_match: Some(vec!["e".into()]),
            if_unmodified_since: Some(t(0)),
            ..Default::default()
        };
        assert_eq!(evaluate(&c, "e", lm), CondResult::Proceed, "If-Match true overrides IUS");
        let c = ReadConditions {
            if_none_match: Some(vec!["x".into()]),
            if_modified_since: Some(t(2_000_000_000)),
            ..Default::default()
        };
        assert_eq!(evaluate(&c, "e", lm), CondResult::Proceed, "INM false wins over IMS");
        let c = ReadConditions {
            if_none_match: Some(vec!["e".into()]),
            ..Default::default()
        };
        assert_eq!(evaluate(&c, "e", lm), CondResult::NotModified);
        let c = ReadConditions {
            if_modified_since: Some(t(1_000_000_000)),
            ..Default::default()
        };
        assert_eq!(evaluate(&c, "e", lm), CondResult::NotModified, "same second is not modified");
        let c = ReadConditions {
            if_match: Some(vec!["nope".into()]),
            ..Default::default()
        };
        assert_eq!(evaluate(&c, "e", lm), CondResult::PreconditionFailed);
    }

    #[test]
    fn formats() {
        assert_eq!(iso8601(1_255_369_830_000), "2009-10-12T17:50:30.000Z");
        assert_eq!(http_date(1_255_369_830_000), "Mon, 12 Oct 2009 17:50:30 GMT");
        assert_eq!(etag_list("\"a\", W/\"b\",*"), vec!["a", "b", "*"]);
    }
}
