//! Explicit upload integrity: `Content-MD5`, `x-amz-checksum-*` headers and
//! trailers, and `x-amz-sdk-checksum-algorithm`. Independent of SigV4.

use base64::Engine;

use super::auth::PayloadDecl;
use super::error::{S3Error, S3Result};
use super::request::S3Request;
use crate::checksums::{Algorithm, Digests, decode_digest};

#[derive(Debug, Default, Clone)]
pub struct ChecksumRequest {
    /// `x-amz-checksum-<alg>` request header value (decoded).
    pub header: Option<(Algorithm, Vec<u8>)>,
    /// Algorithm whose value arrives as an aws-chunked trailer.
    pub trailer: Option<Algorithm>,
    /// `x-amz-sdk-checksum-algorithm`.
    pub sdk: Option<Algorithm>,
    pub content_md5: Option<[u8; 16]>,
}

impl ChecksumRequest {
    pub fn parse(req: &S3Request, payload: &PayloadDecl) -> S3Result<Self> {
        let mut out = ChecksumRequest::default();
        for alg in Algorithm::ALL {
            if let Some(v) = req.header(alg.header_name())? {
                if out.header.is_some() {
                    return Err(S3Error::invalid_request(
                        "Expecting a single x-amz-checksum- header. Multiple checksum Types are not allowed.",
                    ));
                }
                let d = decode_digest(alg, v).ok_or_else(|| {
                    S3Error::invalid_request(format!(
                        "Value for {} header is invalid.",
                        alg.header_name()
                    ))
                })?;
                out.header = Some((alg, d));
            }
        }
        if let Some(t) = req.header("x-amz-trailer")? {
            let name = t.trim().to_ascii_lowercase();
            let alg = Algorithm::from_header_name(&name).ok_or_else(|| {
                S3Error::invalid_request(format!(
                    "The value specified in the x-amz-trailer header is not supported: {t}"
                ))
            })?;
            if !payload.has_trailer() {
                return Err(S3Error::invalid_request(
                    "x-amz-trailer requires a STREAMING-*-TRAILER x-amz-content-sha256 value",
                ));
            }
            if out.header.is_some() {
                return Err(S3Error::invalid_request(
                    "A checksum cannot be sent both as a header and as a trailer",
                ));
            }
            out.trailer = Some(alg);
        } else if payload.has_trailer() {
            return Err(S3Error::invalid_request(
                "x-amz-trailer header is required for trailer payloads",
            ));
        }
        if let Some(s) = req.header("x-amz-sdk-checksum-algorithm")? {
            let alg = Algorithm::parse(s).ok_or_else(|| {
                S3Error::invalid_request(format!("Checksum algorithm {s} is not supported"))
            })?;
            let declared = out.header.as_ref().map(|(a, _)| *a).or(out.trailer);
            if declared.is_some_and(|d| d != alg) {
                return Err(S3Error::invalid_request(
                    "x-amz-sdk-checksum-algorithm does not match the provided checksum",
                ));
            }
            out.sdk = Some(alg);
        }
        if let Some(v) = req.header("content-md5")? {
            let d = base64::engine::general_purpose::STANDARD
                .decode(v.trim())
                .ok()
                .and_then(|d| <[u8; 16]>::try_from(d).ok())
                .ok_or_else(S3Error::invalid_digest)?;
            out.content_md5 = Some(d);
        }
        Ok(out)
    }

    /// The explicitly requested S3 algorithm, if any.
    pub fn algorithm(&self) -> Option<Algorithm> {
        self.header
            .as_ref()
            .map(|(a, _)| *a)
            .or(self.trailer)
            .or(self.sdk)
    }

    /// Algorithms to compute while streaming (plus any extra required).
    pub fn algorithms(&self, extra: &[Algorithm]) -> Vec<Algorithm> {
        let mut v: Vec<Algorithm> = extra.to_vec();
        if let Some(a) = self.algorithm() {
            v.push(a);
        }
        v.sort();
        v.dedup();
        v
    }

    /// Verify all supplied integrity values against computed digests.
    /// Returns the verified explicit S3 checksum `(algorithm, digest)`.
    pub fn verify(
        &self,
        digests: &Digests,
        trailers: &[(String, String)],
    ) -> S3Result<Option<(Algorithm, Vec<u8>)>> {
        if let Some(md5) = &self.content_md5
            && md5 != &digests.md5
        {
            return Err(S3Error::bad_digest());
        }
        let expected = match (&self.header, self.trailer) {
            (Some((alg, d)), _) => Some((*alg, d.clone())),
            (None, Some(alg)) => {
                let v = trailers
                    .iter()
                    .find(|(k, _)| k == alg.header_name())
                    .map(|(_, v)| v.as_str())
                    .ok_or_else(|| S3Error::invalid_request("missing checksum trailer"))?;
                let d = decode_digest(alg, v).ok_or_else(|| {
                    S3Error::invalid_request(format!(
                        "Value for {} trailing header is invalid.",
                        alg.header_name()
                    ))
                })?;
                Some((alg, d))
            }
            (None, None) => None,
        };
        if let Some((alg, want)) = &expected {
            let got = digests.get(*alg).ok_or_else(S3Error::internal)?;
            if got != want.as_slice() {
                return Err(S3Error::bad_checksum(alg.as_str()));
            }
        }
        Ok(expected)
    }
}
