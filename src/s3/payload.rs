//! Request body decoding and verification for every supported payload mode.
//!
//! Data frames are consumed directly; HTTP-level trailer frames are rejected
//! instead of being silently discarded. `aws-chunked` framing, chunk
//! signatures, signed/unsigned checksum trailers, decoded length, and the
//! single-chunk payload hash are all verified before end-of-stream is
//! reported. Framing overhead is bounded independently of object size.

use std::time::Duration;

use axum::body::Body;
use bytes::{Buf, Bytes, BytesMut};
use http_body_util::BodyExt;
use sha2::{Digest, Sha256};

use super::auth::{PayloadDecl, SigningCtx};
use super::error::{S3Error, S3Result};
use crate::sigv4;
use crate::store::ChunkSource;

const MAX_LINE: usize = 4096;
const MAX_TRAILER_LINES: usize = 8;

enum State {
    Header,
    Data {
        remaining: u64,
        hasher: Option<Sha256>,
        signature: Option<String>,
    },
    DataCrlf {
        hasher: Option<Sha256>,
        signature: Option<String>,
    },
    FinalCrlf,
    Trailers,
    Done,
}

pub struct Payload {
    body: Body,
    mode: PayloadDecl,
    signing: Option<SigningCtx>,
    prev_sig: String,
    idle: Duration,
    /// Decoded length the client declared.
    expected_len: Option<u64>,
    trailer_name: Option<String>,
    sha: Option<Sha256>,
    raw: BytesMut,
    input_done: bool,
    state: State,
    decoded: u64,
    overhead: u64,
    overhead_budget: u64,
    trailer_lines: Vec<(String, String)>,
    trailer_signature: Option<String>,
    finished: bool,
}

impl Payload {
    /// `expected_len` is `x-amz-decoded-content-length` for streaming modes
    /// and `Content-Length` otherwise.
    pub fn new(
        body: Body,
        mode: PayloadDecl,
        signing: Option<SigningCtx>,
        expected_len: Option<u64>,
        trailer_name: Option<String>,
        idle: Duration,
    ) -> Self {
        let sha = matches!(mode, PayloadDecl::Sha256(_)).then(Sha256::new);
        let prev_sig = signing
            .as_ref()
            .map(|s| s.seed_signature.clone())
            .unwrap_or_default();
        let overhead_budget = 64 * 1024 + expected_len.unwrap_or(0) / 8;
        Self {
            body,
            mode,
            signing,
            prev_sig,
            idle,
            expected_len,
            trailer_name,
            sha,
            raw: BytesMut::new(),
            input_done: false,
            state: State::Header,
            decoded: 0,
            overhead: 0,
            overhead_budget,
            trailer_lines: Vec::new(),
            trailer_signature: None,
            finished: false,
        }
    }

    /// Verified trailing headers (`x-amz-checksum-*`) after end of stream.
    pub fn trailers(&self) -> &[(String, String)] {
        &self.trailer_lines
    }

    pub fn decoded_len(&self) -> u64 {
        self.decoded
    }

    /// Collect a small body (XML/config) with a hard size limit.
    pub async fn collect(&mut self, limit: usize) -> S3Result<Bytes> {
        if self.expected_len.is_some_and(|l| l > limit as u64) {
            return Err(S3Error::entity_too_large().with_detail("control body exceeds limit"));
        }
        let mut out = BytesMut::new();
        while let Some(chunk) = self.next_chunk().await? {
            if out.len() + chunk.len() > limit {
                return Err(S3Error::entity_too_large().with_detail("control body exceeds limit"));
            }
            out.extend_from_slice(&chunk);
        }
        Ok(out.freeze())
    }

    async fn read_frame(&mut self) -> S3Result<Option<Bytes>> {
        if self.input_done {
            return Ok(None);
        }
        let frame = match tokio::time::timeout(self.idle, self.body.frame()).await {
            Err(_) => return Err(S3Error::request_timeout()),
            Ok(None) => {
                self.input_done = true;
                return Ok(None);
            }
            Ok(Some(Err(e))) => {
                return Err(
                    S3Error::incomplete_body().with_detail(format!("body read failed: {e}"))
                );
            }
            Ok(Some(Ok(f))) => f,
        };
        match frame.into_data() {
            Ok(data) => Ok(Some(data)),
            Err(_) => Err(S3Error::invalid_request(
                "HTTP trailer fields are not supported; use aws-chunked checksum trailers",
            )),
        }
    }

    async fn next_plain(&mut self) -> S3Result<Option<Bytes>> {
        loop {
            match self.read_frame().await? {
                Some(data) if data.is_empty() => continue,
                Some(data) => {
                    self.decoded += data.len() as u64;
                    if let Some(h) = self.sha.as_mut() {
                        h.update(&data);
                    }
                    return Ok(Some(data));
                }
                None => {
                    if let Some(expected) = self.expected_len
                        && expected != self.decoded
                    {
                        return Err(S3Error::incomplete_body());
                    }
                    if let (PayloadDecl::Sha256(want), Some(h)) = (&self.mode, self.sha.take()) {
                        let got: [u8; 32] = h.finalize().into();
                        if &got != want {
                            return Err(S3Error::content_sha256_mismatch());
                        }
                    }
                    self.finished = true;
                    return Ok(None);
                }
            }
        }
    }

    fn charge_overhead(&mut self, n: usize) -> S3Result<()> {
        self.overhead += n as u64;
        if self.overhead > self.overhead_budget {
            return Err(S3Error::invalid_request(
                "aws-chunked framing overhead exceeds the allowed bound",
            ));
        }
        Ok(())
    }

    /// Take one CRLF-terminated line from the raw buffer, if complete.
    fn take_line(&mut self) -> S3Result<Option<String>> {
        let Some(pos) = self.raw.windows(2).position(|w| w == b"\r\n") else {
            if self.raw.len() > MAX_LINE {
                return Err(S3Error::invalid_request("aws-chunked line too long"));
            }
            return Ok(None);
        };
        if pos > MAX_LINE {
            return Err(S3Error::invalid_request("aws-chunked line too long"));
        }
        let line = self.raw.split_to(pos);
        self.raw.advance(2);
        self.charge_overhead(pos + 2)?;
        String::from_utf8(line.to_vec())
            .map(Some)
            .map_err(|_| S3Error::invalid_request("aws-chunked line is not valid UTF-8"))
    }

    fn signed(&self) -> bool {
        matches!(
            self.mode,
            PayloadDecl::StreamingSigned | PayloadDecl::StreamingSignedTrailer
        )
    }

    fn verify_chunk_signature(&mut self, chunk_sha_hex: &str, provided: &str) -> S3Result<()> {
        let ctx = self.signing.as_ref().ok_or_else(S3Error::internal)?;
        let sts =
            sigv4::chunk_string_to_sign(&ctx.amz_date, &ctx.scope, &self.prev_sig, chunk_sha_hex);
        let mac = sigv4::hmac(&ctx.key, sts.as_bytes());
        if !sigv4::signature_eq(&mac, provided) {
            return Err(S3Error::signature_does_not_match().with_detail("chunk signature mismatch"));
        }
        self.prev_sig = provided.to_ascii_lowercase();
        Ok(())
    }

    fn parse_header(&mut self, line: &str) -> S3Result<()> {
        let (size_hex, ext) = match line.split_once(';') {
            Some((s, e)) => (s, Some(e)),
            None => (line, None),
        };
        if size_hex.is_empty()
            || size_hex.len() > 16
            || !size_hex.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(S3Error::invalid_request("invalid aws-chunked chunk size"));
        }
        let size = u64::from_str_radix(size_hex, 16)
            .map_err(|_| S3Error::invalid_request("invalid chunk size"))?;
        let signature = if self.signed() {
            let sig = ext
                .and_then(|e| e.strip_prefix("chunk-signature="))
                .ok_or_else(|| S3Error::invalid_request("missing chunk-signature"))?;
            if sig.len() != 64 {
                return Err(
                    S3Error::signature_does_not_match().with_detail("malformed chunk signature")
                );
            }
            Some(sig.to_string())
        } else {
            if ext.is_some() {
                return Err(S3Error::invalid_request("unexpected aws-chunked extension"));
            }
            None
        };
        let total = self
            .decoded
            .checked_add(size)
            .ok_or_else(|| S3Error::invalid_request("chunk size overflow"))?;
        if self.expected_len.is_some_and(|e| total > e) {
            return Err(S3Error::invalid_request(
                "chunked body exceeds x-amz-decoded-content-length",
            ));
        }
        if size == 0 {
            if let Some(sig) = &signature {
                let sig = sig.clone();
                self.verify_chunk_signature(sigv4::EMPTY_SHA256, &sig)?;
            }
            self.state = if self.mode.has_trailer() {
                State::Trailers
            } else {
                State::FinalCrlf
            };
        } else {
            let hasher = signature.as_ref().map(|_| Sha256::new());
            self.state = State::Data {
                remaining: size,
                hasher,
                signature,
            };
        }
        Ok(())
    }

    fn finish_trailers(&mut self) -> S3Result<()> {
        let declared = self.trailer_name.clone().ok_or_else(|| {
            S3Error::invalid_request("x-amz-trailer header is required for trailer payloads")
        })?;
        let names: Vec<&str> = self.trailer_lines.iter().map(|(k, _)| k.as_str()).collect();
        if names != [declared.as_str()] {
            return Err(S3Error::invalid_request(
                "trailing headers do not match the declared x-amz-trailer",
            ));
        }
        match (&self.mode, self.trailer_signature.take()) {
            (PayloadDecl::StreamingSignedTrailer, Some(sig)) => {
                let canonical: String = self
                    .trailer_lines
                    .iter()
                    .map(|(k, v)| format!("{k}:{v}\n"))
                    .collect();
                let ctx = self.signing.as_ref().ok_or_else(S3Error::internal)?;
                let sts = sigv4::trailer_string_to_sign(
                    &ctx.amz_date,
                    &ctx.scope,
                    &self.prev_sig,
                    &sigv4::sha256_hex(canonical.as_bytes()),
                );
                if !sigv4::signature_eq(&sigv4::hmac(&ctx.key, sts.as_bytes()), &sig) {
                    return Err(S3Error::signature_does_not_match()
                        .with_detail("trailer signature mismatch"));
                }
            }
            (PayloadDecl::StreamingSignedTrailer, None) => {
                return Err(S3Error::signature_does_not_match()
                    .with_detail("missing x-amz-trailer-signature"));
            }
            (_, Some(_)) => {
                return Err(S3Error::invalid_request(
                    "unexpected trailer signature on an unsigned payload",
                ));
            }
            _ => {}
        }
        Ok(())
    }

    async fn next_streaming(&mut self) -> S3Result<Option<Bytes>> {
        loop {
            match &mut self.state {
                State::Done => {
                    if !self.raw.is_empty() {
                        return Err(S3Error::invalid_request(
                            "data after the final aws-chunked frame",
                        ));
                    }
                    // The input must end here.
                    if let Some(extra) = self.read_frame().await?
                        && !extra.is_empty()
                    {
                        return Err(S3Error::invalid_request(
                            "data after the final aws-chunked frame",
                        ));
                    }
                    if !self.input_done {
                        continue;
                    }
                    if self.expected_len.is_some_and(|e| e != self.decoded) {
                        return Err(S3Error::incomplete_body());
                    }
                    self.finished = true;
                    return Ok(None);
                }
                State::Data {
                    remaining, hasher, ..
                } if !self.raw.is_empty() => {
                    let n = (*remaining).min(self.raw.len() as u64) as usize;
                    let data = self.raw.split_to(n).freeze();
                    if let Some(h) = hasher.as_mut() {
                        h.update(&data);
                    }
                    *remaining -= n as u64;
                    if *remaining == 0
                        && let State::Data {
                            hasher, signature, ..
                        } = std::mem::replace(&mut self.state, State::Header)
                    {
                        self.state = State::DataCrlf { hasher, signature };
                    }
                    self.decoded += n as u64;
                    return Ok(Some(data));
                }
                State::DataCrlf { .. } if self.raw.len() >= 2 => {
                    if &self.raw[..2] != b"\r\n" {
                        return Err(S3Error::invalid_request("missing CRLF after chunk data"));
                    }
                    self.raw.advance(2);
                    self.charge_overhead(2)?;
                    if let State::DataCrlf { hasher, signature } =
                        std::mem::replace(&mut self.state, State::Header)
                        && let (Some(h), Some(sig)) = (hasher, signature)
                    {
                        let hexsum = hex::encode(h.finalize());
                        self.verify_chunk_signature(&hexsum, &sig)?;
                    }
                    continue;
                }
                State::FinalCrlf if self.raw.len() >= 2 => {
                    if &self.raw[..2] != b"\r\n" {
                        return Err(S3Error::invalid_request("missing final CRLF"));
                    }
                    self.raw.advance(2);
                    self.charge_overhead(2)?;
                    self.state = State::Done;
                    continue;
                }
                State::Header => {
                    if let Some(line) = self.take_line()? {
                        self.parse_header(&line)?;
                        continue;
                    }
                }
                State::Trailers => {
                    if let Some(line) = self.take_line()? {
                        if line.is_empty() {
                            self.finish_trailers()?;
                            self.state = State::Done;
                            continue;
                        }
                        if self.trailer_lines.len() + usize::from(self.trailer_signature.is_some())
                            >= MAX_TRAILER_LINES
                        {
                            return Err(S3Error::invalid_request("too many trailing headers"));
                        }
                        let (k, v) = line
                            .split_once(':')
                            .ok_or_else(|| S3Error::invalid_request("malformed trailing header"))?;
                        let k = k.trim().to_ascii_lowercase();
                        let v = v.trim().to_string();
                        if self.trailer_signature.is_some() {
                            return Err(S3Error::invalid_request(
                                "trailing header after trailer signature",
                            ));
                        }
                        if k == "x-amz-trailer-signature" {
                            self.trailer_signature = Some(v);
                        } else {
                            if self.trailer_lines.iter().any(|(n, _)| *n == k) {
                                return Err(S3Error::invalid_request("duplicate trailing header"));
                            }
                            self.trailer_lines.push((k, v));
                        }
                        continue;
                    }
                }
                _ => {}
            }
            // Need more input.
            match self.read_frame().await? {
                Some(data) => self.raw.extend_from_slice(&data),
                None => {
                    return Err(
                        S3Error::incomplete_body().with_detail("aws-chunked body ended early")
                    );
                }
            }
        }
    }
}

impl ChunkSource for Payload {
    async fn next_chunk(&mut self) -> S3Result<Option<Bytes>> {
        if self.finished {
            return Ok(None);
        }
        if self.mode.is_streaming() {
            self.next_streaming().await
        } else {
            self.next_plain().await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(seed: &str) -> SigningCtx {
        SigningCtx {
            key: sigv4::signing_key(
                "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
                "20130524",
                "us-east-1",
                "s3",
            ),
            amz_date: "20130524T000000Z".into(),
            scope: "20130524/us-east-1/s3/aws4_request".into(),
            seed_signature: seed.into(),
        }
    }

    fn body_from_parts(parts: Vec<Vec<u8>>) -> Body {
        let stream = futures_util::stream::iter(
            parts
                .into_iter()
                .map(|p| Ok::<_, std::io::Error>(Bytes::from(p))),
        );
        Body::from_stream(stream)
    }

    async fn drain(p: &mut Payload) -> S3Result<Vec<u8>> {
        let mut out = Vec::new();
        while let Some(c) = p.next_chunk().await? {
            out.extend_from_slice(&c);
        }
        Ok(out)
    }

    fn aws_trailer_body() -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(b"10000;chunk-signature=b474d8862b1487a5145d686f57f013e54db672cee1c953b3010fb58501ef5aa2\r\n");
        b.extend(std::iter::repeat_n(b'a', 65536));
        b.extend_from_slice(b"\r\n400;chunk-signature=1c1344b170168f8e65b41376b44b20fe354e373826ccbbe2c1d40a8cae51e5c7\r\n");
        b.extend(std::iter::repeat_n(b'a', 1024));
        b.extend_from_slice(b"\r\n0;chunk-signature=2ca2aba2005185cf7159c6277faf83795951dd77a3a99e6e65d5c9f85863f992\r\n");
        b.extend_from_slice(b"x-amz-checksum-crc32c:sOO8/Q==\r\n");
        b.extend_from_slice(b"x-amz-trailer-signature:d81f82fc3505edab99d459891051a732e8730629a2e4a59689829ca17fe2e435\r\n\r\n");
        b
    }

    fn signed_trailer_payload(body: Vec<u8>, split: usize) -> Payload {
        let parts: Vec<Vec<u8>> = body.chunks(split).map(|c| c.to_vec()).collect();
        Payload::new(
            body_from_parts(parts),
            PayloadDecl::StreamingSignedTrailer,
            Some(ctx(
                "106e2a8a18243abcf37539882f36619c00e2dfc72633413f02d3b74544bfeb8e",
            )),
            Some(66560),
            Some("x-amz-checksum-crc32c".into()),
            Duration::from_secs(5),
        )
    }

    #[tokio::test]
    async fn aws_signed_trailer_example_decodes_at_any_split() {
        for split in [1usize, 7, 100, 4096, 70_000] {
            let mut p = signed_trailer_payload(aws_trailer_body(), split);
            let data = drain(&mut p).await.unwrap();
            assert_eq!(data.len(), 66560);
            assert!(data.iter().all(|b| *b == b'a'));
            assert_eq!(
                p.trailers(),
                &[("x-amz-checksum-crc32c".to_string(), "sOO8/Q==".to_string())]
            );
        }
    }

    #[tokio::test]
    async fn tampered_chunk_or_trailer_fails() {
        let mut body = aws_trailer_body();
        let i = 200;
        body[i] = b'b';
        assert!(
            drain(&mut signed_trailer_payload(body, 8192))
                .await
                .is_err()
        );
        let body = String::from_utf8(aws_trailer_body())
            .unwrap()
            .replace("sOO8/Q==", "AAAAAA==")
            .into_bytes();
        let err = drain(&mut signed_trailer_payload(body, 8192))
            .await
            .unwrap_err();
        assert_eq!(err.code, "SignatureDoesNotMatch");
        let mut body = aws_trailer_body();
        body.extend_from_slice(b"junk");
        assert!(
            drain(&mut signed_trailer_payload(body, 8192))
                .await
                .is_err()
        );
        let body = aws_trailer_body();
        let truncated = body[..body.len() - 2].to_vec();
        assert!(
            drain(&mut signed_trailer_payload(truncated, 8192))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn undeclared_trailer_is_rejected() {
        let body = aws_trailer_body();
        let mut p = signed_trailer_payload(body, 8192);
        p.trailer_name = Some("x-amz-checksum-crc32".into());
        assert!(drain(&mut p).await.is_err());
    }

    #[tokio::test]
    async fn unsigned_trailer_mode() {
        let body =
            b"5\r\nhello\r\n6\r\n world\r\n0\r\nx-amz-checksum-crc32:DUoRhQ==\r\n\r\n".to_vec();
        let mut p = Payload::new(
            body_from_parts(vec![body]),
            PayloadDecl::StreamingUnsignedTrailer,
            None,
            Some(11),
            Some("x-amz-checksum-crc32".into()),
            Duration::from_secs(5),
        );
        assert_eq!(drain(&mut p).await.unwrap(), b"hello world");
        assert_eq!(p.trailers()[0].1, "DUoRhQ==");
        // Decoded length mismatch.
        let body = b"5\r\nhello\r\n0\r\nx-amz-checksum-crc32:x\r\n\r\n".to_vec();
        let mut p = Payload::new(
            body_from_parts(vec![body]),
            PayloadDecl::StreamingUnsignedTrailer,
            None,
            Some(6),
            Some("x-amz-checksum-crc32".into()),
            Duration::from_secs(5),
        );
        assert!(drain(&mut p).await.is_err());
    }

    #[tokio::test]
    async fn signed_single_chunk_hash_is_verified() {
        let want: [u8; 32] = Sha256::digest(b"abc").into();
        let mut p = Payload::new(
            body_from_parts(vec![b"ab".to_vec(), b"c".to_vec()]),
            PayloadDecl::Sha256(want),
            None,
            Some(3),
            None,
            Duration::from_secs(5),
        );
        assert_eq!(drain(&mut p).await.unwrap(), b"abc");
        let mut p = Payload::new(
            body_from_parts(vec![b"abd".to_vec()]),
            PayloadDecl::Sha256(want),
            None,
            Some(3),
            None,
            Duration::from_secs(5),
        );
        assert_eq!(
            drain(&mut p).await.unwrap_err().code,
            "XAmzContentSHA256Mismatch"
        );
    }

    #[tokio::test]
    async fn framing_overhead_is_bounded() {
        // One-byte chunks: overhead quickly exceeds the budget.
        let mut body = Vec::new();
        for _ in 0..40_000 {
            body.extend_from_slice(b"1\r\na\r\n");
        }
        body.extend_from_slice(b"0\r\nx-amz-checksum-crc32:x\r\n\r\n");
        let mut p = Payload::new(
            body_from_parts(vec![body]),
            PayloadDecl::StreamingUnsignedTrailer,
            None,
            Some(40_000),
            Some("x-amz-checksum-crc32".into()),
            Duration::from_secs(5),
        );
        assert!(drain(&mut p).await.is_err());
    }
}
