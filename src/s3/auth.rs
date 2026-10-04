//! Request authentication: SigV4 header and presigned-query verification.
//! There is no anonymous fallback; every S3 request except a CORS preflight
//! must carry a valid signature from an enabled, unexpired credential.

use std::sync::Arc;

use super::error::{S3Error, S3Result};
use super::request::S3Request;
use crate::credentials::{Credential, CredentialSet};
use crate::sigv4::{self, AuthorizationHeader, CredentialScope};

/// What the client declared about the payload (`x-amz-content-sha256`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PayloadDecl {
    Sha256([u8; 32]),
    Unsigned,
    StreamingSigned,
    StreamingSignedTrailer,
    StreamingUnsignedTrailer,
}

impl PayloadDecl {
    pub fn parse(v: &str) -> S3Result<Self> {
        Ok(match v {
            "UNSIGNED-PAYLOAD" => Self::Unsigned,
            "STREAMING-AWS4-HMAC-SHA256-PAYLOAD" => Self::StreamingSigned,
            "STREAMING-AWS4-HMAC-SHA256-PAYLOAD-TRAILER" => Self::StreamingSignedTrailer,
            "STREAMING-UNSIGNED-PAYLOAD-TRAILER" => Self::StreamingUnsignedTrailer,
            s if s.starts_with("STREAMING-") => {
                return Err(S3Error::not_implemented(format!("payload signing mode {s} is not supported")));
            }
            s => {
                let mut out = [0u8; 32];
                if s.len() != 64 || hex::decode_to_slice(s.to_ascii_lowercase(), &mut out).is_err() {
                    return Err(S3Error::invalid_argument("x-amz-content-sha256 must be a SHA-256 hex digest or a supported mode"));
                }
                Self::Sha256(out)
            }
        })
    }

    pub fn is_streaming(&self) -> bool {
        matches!(
            self,
            Self::StreamingSigned | Self::StreamingSignedTrailer | Self::StreamingUnsignedTrailer
        )
    }

    pub fn has_trailer(&self) -> bool {
        matches!(self, Self::StreamingSignedTrailer | Self::StreamingUnsignedTrailer)
    }
}

/// Material needed to verify chunk and trailer signatures.
#[derive(Clone)]
pub struct SigningCtx {
    pub key: [u8; 32],
    pub amz_date: String,
    pub scope: String,
    pub seed_signature: String,
}

pub struct AuthContext {
    pub credential: Arc<Credential>,
    pub payload: PayloadDecl,
    pub signing: SigningCtx,
    pub presigned: bool,
}

impl std::fmt::Debug for AuthContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthContext")
            .field("credential", &self.credential.id)
            .field("presigned", &self.presigned)
            .finish()
    }
}

pub struct AuthParams<'a> {
    pub region: &'a str,
    pub max_skew_secs: i64,
    pub now_secs: i64,
    pub now_ms: i64,
}

/// Header names that may be present without being signed.
fn unsigned_ok(name: &str) -> bool {
    name == "x-amz-user-agent"
}

/// Reject signature schemes this service never accepts (checked before any
/// other request validation so the client gets a precise error).
pub fn reject_unsupported_schemes(req: &S3Request) -> S3Result<()> {
    if req.has_q("Signature") || req.has_q("AWSAccessKeyId") {
        return Err(S3Error::invalid_request(
            "The authorization mechanism you have provided is not supported. Please use AWS4-HMAC-SHA256.",
        ));
    }
    Ok(())
}

pub fn authenticate(req: &S3Request, creds: &CredentialSet, p: &AuthParams<'_>) -> S3Result<AuthContext> {
    let has_header = req.headers.contains_key(http::header::AUTHORIZATION);
    let has_query = req.has_q("X-Amz-Signature") || req.has_q("X-Amz-Algorithm") || req.has_q("X-Amz-Credential");
    reject_unsupported_schemes(req)?;
    if req.headers.contains_key("x-amz-security-token") || req.has_q("X-Amz-Security-Token") {
        return Err(S3Error::invalid_request("Temporary security credentials are not supported by this service."));
    }
    match (has_header, has_query) {
        (true, true) => Err(S3Error::invalid_argument(
            "Only one auth mechanism allowed; only the X-Amz-Algorithm query parameter or the Authorization header should be specified",
        )),
        (false, false) => Err(S3Error::access_denied().with_detail("anonymous request")),
        (true, false) => header_auth(req, creds, p),
        (false, true) => query_auth(req, creds, p),
    }
}

fn header_auth(req: &S3Request, creds: &CredentialSet, p: &AuthParams<'_>) -> S3Result<AuthContext> {
    let value = req
        .header("authorization")?
        .ok_or_else(S3Error::access_denied)?;
    if value.starts_with("AWS ") {
        return Err(S3Error::invalid_request(
            "The authorization mechanism you have provided is not supported. Please use AWS4-HMAC-SHA256.",
        ));
    }
    let auth = AuthorizationHeader::parse(value).map_err(|e| {
        if e == "unsupported authorization algorithm" {
            S3Error::invalid_request("The authorization mechanism you have provided is not supported. Please use AWS4-HMAC-SHA256.")
        } else {
            S3Error::authorization_header_malformed(e)
        }
    })?;
    let amz_date = req
        .header("x-amz-date")?
        .ok_or_else(|| S3Error::access_denied().with_detail("missing x-amz-date"))?
        .to_string();
    let t = sigv4::parse_amz_date(&amz_date)
        .ok_or_else(|| S3Error::access_denied().with_detail("invalid x-amz-date"))?;
    if (p.now_secs - t).abs() > p.max_skew_secs {
        return Err(S3Error::request_time_too_skewed());
    }
    let payload_hash = req
        .header("x-amz-content-sha256")?
        .ok_or_else(|| S3Error::invalid_request("Missing required header for this request: x-amz-content-sha256"))?
        .to_string();
    let payload = PayloadDecl::parse(&payload_hash)?;
    verify(
        req,
        creds,
        p,
        &auth.credential,
        &auth.signed_headers,
        &auth.signature,
        &amz_date,
        &payload_hash,
        &req.raw_query,
        payload,
        false,
    )
}

fn query_auth(req: &S3Request, creds: &CredentialSet, p: &AuthParams<'_>) -> S3Result<AuthContext> {
    let get = |n: &str| {
        req.q(n)
            .ok_or_else(|| S3Error::authorization_query_error(format!("Query-string authentication requires {n}")))
    };
    if get("X-Amz-Algorithm")? != sigv4::ALGORITHM {
        return Err(S3Error::authorization_query_error(
            "X-Amz-Algorithm only supports \"AWS4-HMAC-SHA256\"",
        ));
    }
    let credential = CredentialScope::parse(get("X-Amz-Credential")?)
        .ok_or_else(|| S3Error::authorization_query_error("Error parsing the X-Amz-Credential parameter"))?;
    let amz_date = get("X-Amz-Date")?.to_string();
    let t = sigv4::parse_amz_date(&amz_date)
        .ok_or_else(|| S3Error::authorization_query_error("X-Amz-Date must be in the ISO8601 Long Format"))?;
    let expires: i64 = get("X-Amz-Expires")?
        .parse()
        .map_err(|_| S3Error::authorization_query_error("X-Amz-Expires should be a number"))?;
    if !(1..=604_800).contains(&expires) {
        return Err(S3Error::authorization_query_error(
            "X-Amz-Expires must be between 1 and 604800 seconds",
        ));
    }
    if t > p.now_secs + p.max_skew_secs {
        return Err(S3Error::access_denied().with_detail("presigned request is not valid yet"));
    }
    if p.now_secs > t + expires {
        return Err(S3Error::new("AccessDenied", http::StatusCode::FORBIDDEN, "Request has expired"));
    }
    let signed = sigv4::parse_signed_headers(get("X-Amz-SignedHeaders")?)
        .map_err(S3Error::authorization_query_error)?;
    let signature = get("X-Amz-Signature")?.to_string();
    let payload_hash = req.q("X-Amz-Content-Sha256").unwrap_or("UNSIGNED-PAYLOAD").to_string();
    let payload = PayloadDecl::parse(&payload_hash)?;
    if payload.is_streaming() {
        return Err(S3Error::not_implemented("streaming payloads are not supported with presigned URLs"));
    }
    verify(
        req,
        creds,
        p,
        &credential,
        &signed,
        &signature,
        &amz_date,
        &payload_hash,
        &req.raw_query,
        payload,
        true,
    )
}

#[allow(clippy::too_many_arguments)]
fn verify(
    req: &S3Request,
    creds: &CredentialSet,
    p: &AuthParams<'_>,
    scope: &CredentialScope,
    signed_headers: &[String],
    signature: &str,
    amz_date: &str,
    payload_hash: &str,
    raw_query: &str,
    payload: PayloadDecl,
    presigned: bool,
) -> S3Result<AuthContext> {
    if scope.date != amz_date[..8] {
        return Err(S3Error::signature_does_not_match().with_detail("credential scope date mismatch"));
    }
    if scope.service != "s3" {
        return Err(S3Error::authorization_header_malformed(format!(
            "The authorization header is malformed; incorrect service \"{}\". This endpoint belongs to \"s3\".",
            scope.service
        )));
    }
    if scope.region != p.region {
        return Err(S3Error::authorization_header_malformed(format!(
            "The authorization header is malformed; the region '{}' is wrong; expecting '{}'",
            scope.region, p.region
        ))
        .with_extra("Region", p.region));
    }
    let credential = creds
        .get(&scope.access_key)
        .ok_or_else(S3Error::invalid_access_key_id)?;
    if credential.is_expired(p.now_ms) {
        return Err(S3Error::invalid_access_key_id().with_detail("credential expired"));
    }
    if !signed_headers.iter().any(|h| h == "host") {
        return Err(S3Error::access_denied().with_detail("host header must be signed"));
    }
    let mut unsigned: Vec<&str> = req
        .headers
        .keys()
        .map(|k| k.as_str())
        .filter(|k| k.starts_with("x-amz-") && !unsigned_ok(k))
        .filter(|k| !signed_headers.iter().any(|s| s == k))
        .collect();
    unsigned.dedup();
    if !unsigned.is_empty() {
        return Err(S3Error::access_denied().with_detail(format!(
            "There were headers present in the request which were not signed: {}",
            unsigned.join(", ")
        )));
    }
    let Some(block) = sigv4::canonical_headers(&req.headers, signed_headers) else {
        return Err(S3Error::signature_does_not_match().with_detail("a signed header is missing"));
    };
    let query = sigv4::canonical_query(raw_query, presigned.then_some("X-Amz-Signature"));
    let signed_str = signed_headers.join(";");
    let key = sigv4::signing_key(credential.secret(), &scope.date, &scope.region, "s3");
    let scope_str = scope.scope();
    let matched = sigv4::canonical_uri_candidates(&req.raw_path).iter().any(|uri| {
        let cr = sigv4::canonical_request(req.method.as_str(), uri, &query, &block, &signed_str, payload_hash);
        let sts = sigv4::string_to_sign(amz_date, &scope_str, &cr);
        sigv4::signature_eq(&sigv4::hmac(&key, sts.as_bytes()), signature)
    });
    if !matched {
        return Err(S3Error::signature_does_not_match());
    }
    Ok(AuthContext {
        credential,
        payload,
        signing: SigningCtx {
            key,
            amz_date: amz_date.to_string(),
            scope: scope_str,
            seed_signature: signature.to_ascii_lowercase(),
        },
        presigned,
    })
}
