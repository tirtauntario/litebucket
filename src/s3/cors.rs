//! Bucket CORS configuration and browser preflight/actual-request headers.
//!
//! CORS only controls what a browser may read; it never authenticates or
//! authorizes an S3 request. Preflights are unsigned and never mutate state.

use std::sync::Arc;

use axum::body::Body;
use http::{Method, Response, StatusCode};
use serde::{Deserialize, Serialize};

use super::error::{S3Error, S3Result};
use super::headers::{response, xml};
use super::request::S3Request;
use super::xml::XmlWriter;
use super::{Cx, lookup_bucket, read_control_body};
use crate::metadata::queries;
use crate::metadata::with_write_tx;
use crate::store::Store;

const MAX_RULES: usize = 100;
const METHODS: &[&str] = &["GET", "PUT", "POST", "DELETE", "HEAD"];

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CorsRule {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub allowed_origins: Vec<String>,
    pub allowed_methods: Vec<String>,
    #[serde(default)]
    pub allowed_headers: Vec<String>,
    #[serde(default)]
    pub expose_headers: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_age_seconds: Option<u32>,
}

fn wildcard_ok(s: &str) -> bool {
    s.matches('*').count() <= 1 && !s.is_empty() && s.len() <= 1024 && !s.contains(['\r', '\n'])
}

/// Match with at most one `*` wildcard.
fn wildcard_match(pattern: &str, value: &str, ignore_case: bool) -> bool {
    let (p, v) = if ignore_case {
        (pattern.to_ascii_lowercase(), value.to_ascii_lowercase())
    } else {
        (pattern.to_string(), value.to_string())
    };
    match p.split_once('*') {
        None => p == v,
        Some((pre, suf)) => {
            v.len() >= pre.len() + suf.len() && v.starts_with(pre) && v.ends_with(suf)
        }
    }
}

pub fn parse_config(body: &[u8]) -> S3Result<Vec<CorsRule>> {
    let doc = super::xml::parse(body)?;
    if doc.name != "CORSConfiguration" {
        return Err(S3Error::malformed_xml());
    }
    let mut rules = Vec::new();
    for r in &doc.children {
        if r.name != "CORSRule" {
            return Err(S3Error::malformed_xml());
        }
        let mut rule = CorsRule::default();
        for c in &r.children {
            let t = c.text.trim().to_string();
            match c.name.as_str() {
                "ID" => {
                    if t.len() > 255 {
                        return Err(S3Error::invalid_argument("CORS rule ID is too long"));
                    }
                    rule.id = Some(t);
                }
                "AllowedOrigin" => {
                    if !wildcard_ok(&t) {
                        return Err(S3Error::invalid_request(format!(
                            "AllowedOrigin \"{t}\" can not have more than one wildcard."
                        )));
                    }
                    rule.allowed_origins.push(t);
                }
                "AllowedMethod" => {
                    if !METHODS.contains(&t.as_str()) {
                        return Err(S3Error::invalid_request(format!(
                            "Found unsupported HTTP method in CORS config. Unsupported method is {t}"
                        )));
                    }
                    rule.allowed_methods.push(t);
                }
                "AllowedHeader" => {
                    if !wildcard_ok(&t) {
                        return Err(S3Error::invalid_request(format!(
                            "AllowedHeader \"{t}\" can not have more than one wildcard."
                        )));
                    }
                    rule.allowed_headers.push(t);
                }
                "ExposeHeader" => {
                    if t.contains('*')
                        || t.is_empty()
                        || http::HeaderName::from_bytes(t.as_bytes()).is_err()
                    {
                        return Err(S3Error::invalid_request(format!(
                            "ExposeHeader \"{t}\" contains wildcard or is invalid."
                        )));
                    }
                    rule.expose_headers.push(t);
                }
                "MaxAgeSeconds" => {
                    rule.max_age_seconds = Some(
                        t.parse()
                            .map_err(|_| S3Error::invalid_argument("invalid MaxAgeSeconds"))?,
                    );
                }
                _ => return Err(S3Error::malformed_xml()),
            }
        }
        if rule.allowed_origins.is_empty() || rule.allowed_methods.is_empty() {
            return Err(S3Error::malformed_xml());
        }
        rules.push(rule);
    }
    if rules.is_empty() || rules.len() > MAX_RULES {
        return Err(S3Error::malformed_xml().with_detail("CORS rule count out of range"));
    }
    Ok(rules)
}

fn require_manage(cx: &Cx) -> S3Result<()> {
    if cx
        .auth
        .credential
        .allows_manage_bucket(cx.req.bucket_name())
    {
        Ok(())
    } else {
        Err(S3Error::access_denied())
    }
}

pub async fn put_bucket_cors(cx: &Cx, body: Body) -> S3Result<Response<Body>> {
    require_manage(cx)?;
    let bucket = cx.bucket().await?;
    let body =
        read_control_body(cx, body, cx.store.config.limits.max_cors_body_bytes, true).await?;
    let rules = parse_config(&body)?;
    let json = serde_json::to_string(&rules)
        .map_err(|e| S3Error::internal().with_detail(e.to_string()))?;
    let id = bucket.id;
    let ok = cx
        .store
        .db
        .write(move |c| with_write_tx(c, |tx| queries::set_bucket_cors(tx, &id, Some(&json))))
        .await?;
    if !ok {
        return Err(S3Error::no_such_bucket());
    }
    Ok(response(
        StatusCode::OK,
        Vec::<(&str, String)>::new(),
        Body::empty(),
    ))
}

pub async fn get_bucket_cors(cx: &Cx) -> S3Result<Response<Body>> {
    require_manage(cx)?;
    let bucket = cx.bucket().await?;
    let rules = load(bucket.cors_json.as_deref()).ok_or_else(S3Error::no_such_cors)?;
    let mut w = XmlWriter::new();
    w.root("CORSConfiguration");
    for r in &rules {
        w.open("CORSRule");
        w.opt("ID", r.id.as_deref());
        for o in &r.allowed_origins {
            w.elem("AllowedOrigin", o);
        }
        for m in &r.allowed_methods {
            w.elem("AllowedMethod", m);
        }
        for h in &r.allowed_headers {
            w.elem("AllowedHeader", h);
        }
        for h in &r.expose_headers {
            w.elem("ExposeHeader", h);
        }
        if let Some(a) = r.max_age_seconds {
            w.elem("MaxAgeSeconds", &a.to_string());
        }
        w.close("CORSRule");
    }
    w.close("CORSConfiguration");
    Ok(xml(StatusCode::OK, w.finish()))
}

pub async fn delete_bucket_cors(cx: &Cx) -> S3Result<Response<Body>> {
    require_manage(cx)?;
    let bucket = cx.bucket().await?;
    let id = bucket.id;
    cx.store
        .db
        .write(move |c| with_write_tx(c, |tx| queries::set_bucket_cors(tx, &id, None)))
        .await?;
    Ok(response(
        StatusCode::NO_CONTENT,
        Vec::<(&str, String)>::new(),
        Body::empty(),
    ))
}

fn load(json: Option<&str>) -> Option<Vec<CorsRule>> {
    json.and_then(|j| serde_json::from_str(j).ok())
}

fn forbidden() -> S3Error {
    S3Error::new(
        "AccessForbidden",
        StatusCode::FORBIDDEN,
        "CORSResponse: This CORS request is not allowed. This is usually because the evalution of Origin, request method / Access-Control-Request-Method or Access-Control-Request-Headers are not whitelisted by the resource's CORS spec.",
    )
}

fn find_rule<'a>(
    rules: &'a [CorsRule],
    origin: &str,
    method: &str,
    req_headers: &[String],
) -> Option<&'a CorsRule> {
    rules.iter().find(|r| {
        r.allowed_origins
            .iter()
            .any(|o| wildcard_match(o, origin, false))
            && r.allowed_methods.iter().any(|m| m == method)
            && req_headers
                .iter()
                .all(|h| r.allowed_headers.iter().any(|a| wildcard_match(a, h, true)))
    })
}

/// Unsigned OPTIONS preflight. Nonexistent buckets and unmatched requests get
/// the same 403 so existence is not disclosed.
pub async fn preflight(store: &Arc<Store>, req: &S3Request) -> S3Result<Response<Body>> {
    let origin = req.header("origin")?.map(str::to_string);
    let method = req
        .header("access-control-request-method")?
        .map(str::to_string);
    let (Some(origin), Some(method)) = (origin, method) else {
        return Err(S3Error::new(
            "BadRequest",
            StatusCode::BAD_REQUEST,
            "Insufficient information. Origin request header needed.",
        ));
    };
    let req_headers: Vec<String> = req
        .header("access-control-request-headers")?
        .map(|v| {
            v.split(',')
                .map(|h| h.trim().to_ascii_lowercase())
                .filter(|h| !h.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let rules = match lookup_bucket(store, req.bucket_name()).await {
        Ok(b) => load(b.cors_json.as_deref()),
        Err(e) if e.code == "NoSuchBucket" => None,
        Err(e) => return Err(e),
    };
    let rule = rules
        .as_deref()
        .and_then(|r| find_rule(r, &origin, &method, &req_headers))
        .ok_or_else(forbidden)?;
    let star = rule.allowed_origins.iter().any(|o| o == "*");
    let mut h: Vec<(&str, String)> = vec![
        (
            "access-control-allow-origin",
            if star { "*".into() } else { origin.clone() },
        ),
        (
            "access-control-allow-methods",
            rule.allowed_methods.join(", "),
        ),
        (
            "vary",
            "Origin, Access-Control-Request-Headers, Access-Control-Request-Method".into(),
        ),
    ];
    if !req_headers.is_empty() {
        h.push(("access-control-allow-headers", req_headers.join(", ")));
    }
    if !rule.expose_headers.is_empty() {
        h.push((
            "access-control-expose-headers",
            rule.expose_headers.join(", "),
        ));
    }
    if let Some(a) = rule.max_age_seconds {
        h.push(("access-control-max-age", a.to_string()));
    }
    if !star {
        h.push(("access-control-allow-credentials", "true".into()));
    }
    Ok(response(StatusCode::OK, h, Body::empty()))
}

/// Add CORS response headers to an actual (non-preflight) request when a rule
/// matches. Authentication and authorization were already enforced.
pub async fn apply_actual_request_headers(
    store: &Arc<Store>,
    bucket: &str,
    origin: &str,
    method: &Method,
    resp: &mut Response<Body>,
) {
    let Ok(b) = lookup_bucket(store, bucket).await else {
        return;
    };
    let Some(rules) = load(b.cors_json.as_deref()) else {
        return;
    };
    let h = resp.headers_mut();
    h.append(http::header::VARY, http::HeaderValue::from_static("Origin"));
    let Some(rule) = find_rule(&rules, origin, method.as_str(), &[]) else {
        return;
    };
    let star = rule.allowed_origins.iter().any(|o| o == "*");
    let allow = if star { "*" } else { origin };
    if let Ok(v) = http::HeaderValue::from_str(allow) {
        h.insert(http::header::ACCESS_CONTROL_ALLOW_ORIGIN, v);
    }
    if let Ok(v) = http::HeaderValue::from_str(&rule.allowed_methods.join(", ")) {
        h.insert(http::header::ACCESS_CONTROL_ALLOW_METHODS, v);
    }
    if !rule.expose_headers.is_empty()
        && let Ok(v) = http::HeaderValue::from_str(&rule.expose_headers.join(", "))
    {
        h.insert(http::header::ACCESS_CONTROL_EXPOSE_HEADERS, v);
    }
    if !star {
        h.insert(
            http::header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
            http::HeaderValue::from_static("true"),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CFG: &[u8] = br#"<CORSConfiguration>
 <CORSRule>
   <AllowedOrigin>https://*.example.com</AllowedOrigin>
   <AllowedMethod>PUT</AllowedMethod>
   <AllowedMethod>GET</AllowedMethod>
   <AllowedHeader>*</AllowedHeader>
   <ExposeHeader>ETag</ExposeHeader>
   <MaxAgeSeconds>3000</MaxAgeSeconds>
 </CORSRule>
</CORSConfiguration>"#;

    #[test]
    fn parse_and_match() {
        let rules = parse_config(CFG).unwrap();
        assert!(
            find_rule(
                &rules,
                "https://app.example.com",
                "PUT",
                &["content-type".into()]
            )
            .is_some()
        );
        assert!(find_rule(&rules, "https://evil.com", "PUT", &[]).is_none());
        assert!(find_rule(&rules, "https://app.example.com", "DELETE", &[]).is_none());
        assert!(find_rule(&rules, "http://app.example.com", "GET", &[]).is_none());
    }

    #[test]
    fn invalid_configs() {
        for bad in [
            &b"<CORSConfiguration/>"[..],
            b"<CORSConfiguration><CORSRule><AllowedOrigin>*</AllowedOrigin></CORSRule></CORSConfiguration>",
            b"<CORSConfiguration><CORSRule><AllowedOrigin>**</AllowedOrigin><AllowedMethod>GET</AllowedMethod></CORSRule></CORSConfiguration>",
            b"<CORSConfiguration><CORSRule><AllowedOrigin>*</AllowedOrigin><AllowedMethod>PATCH</AllowedMethod></CORSRule></CORSConfiguration>",
            b"<CORSConfiguration><CORSRule><AllowedOrigin>*</AllowedOrigin><AllowedMethod>GET</AllowedMethod><Bogus/></CORSRule></CORSConfiguration>",
        ] {
            assert!(parse_config(bad).is_err(), "{}", String::from_utf8_lossy(bad));
        }
    }
}
