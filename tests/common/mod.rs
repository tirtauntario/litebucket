//! Shared integration-test harness: an in-process server on a temporary data
//! directory plus a small SigV4 client. Everything is local; no cloud
//! endpoint or ambient AWS credential is ever consulted.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};
use storlite::config::Config;
use storlite::credentials::{CredentialSet, CredentialStore};
use storlite::server::Running;
use storlite::sigv4;
use storlite::store::Store;

pub const ADMIN: (&str, &str) = ("admin-key", "adminsecretadminsecretadminsecret01");
pub const APP: (&str, &str) = ("app-key", "appsecretappsecretappsecretappsec01");
pub const READER: (&str, &str) = ("reader-key", "readersecretreadersecretreadersec01");
pub const REGION: &str = "us-east-1";

pub const CREDENTIALS: &str = r#"
[[credentials]]
id = "admin-key"
secret_access_key = "adminsecretadminsecretadminsecret01"
enabled = true
global_grants = ["admin"]

[[credentials]]
id = "app-key"
secret_access_key = "appsecretappsecretappsecretappsec01"
enabled = true
global_grants = ["list_buckets"]
[[credentials.grants]]
bucket = "docs"
prefix = ""
actions = ["read", "list", "write", "delete"]

[[credentials]]
id = "reader-key"
secret_access_key = "readersecretreadersecretreadersec01"
enabled = true
global_grants = []
[[credentials.grants]]
bucket = "docs"
prefix = "customers/123/"
actions = ["read", "list"]
"#;

pub struct TestServer {
    pub dir: tempfile::TempDir,
    pub config_path: PathBuf,
    pub running: Option<Running>,
    pub base: String,
    pub mgmt: String,
}

/// Tests pin a small disk reserve so results do not depend on how full the
/// host disk is (the production default is max(1 GiB, 5%)).
pub fn with_test_limits(extra: &str) -> String {
    const FLOOR: &str = "min_disk_free_percent = 0\nmin_disk_free_bytes = 67108864\n";
    if extra.contains("[limits]\n") {
        extra.replacen("[limits]\n", &format!("[limits]\n{FLOOR}"), 1)
    } else {
        format!("{extra}\n[limits]\n{FLOOR}")
    }
}

pub fn write_config(dir: &Path, extra: &str) -> PathBuf {
    let extra = with_test_limits(extra);
    let cfg = format!(
        r#"data_dir = "./data"
credentials_file = "./credentials.toml"
[http]
listen = "127.0.0.1:0"
allow_insecure_loopback_http = true
[management]
listen = "127.0.0.1:0"
[logging]
format = "text"
level = "warn"
{extra}
"#
    );
    let path = dir.join("config.toml");
    std::fs::write(&path, cfg).unwrap();
    let creds = dir.join("credentials.toml");
    if !creds.exists() {
        std::fs::write(&creds, CREDENTIALS).unwrap();
        std::fs::set_permissions(&creds, std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .unwrap();
    }
    path
}

/// Merge extra TOML sections into the config text. `extra` may contain
/// `[limits]`-style sections; duplicate sections are merged by the caller.
pub fn load_config(path: &Path) -> Config {
    Config::load(path, &Default::default()).unwrap()
}

impl TestServer {
    pub async fn start() -> Self {
        Self::start_with("").await
    }

    /// `extra` is appended to the config file (e.g. a `[limits]` section).
    pub async fn start_with(extra: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_config(dir.path(), extra);
        let cfg = load_config(&config_path);
        storlite::store::initialize(&cfg).unwrap();
        let mut s = Self {
            dir,
            config_path,
            running: None,
            base: String::new(),
            mgmt: String::new(),
        };
        s.boot().await;
        s
    }

    pub async fn boot(&mut self) {
        let cfg = load_config(&self.config_path);
        let creds = CredentialSet::load(&cfg.credentials_file, false).unwrap();
        let store = Store::open(cfg, CredentialStore::new(creds)).unwrap();
        let running = storlite::server::start(store).await.unwrap();
        self.base = format!("http://{}", running.s3_addr);
        self.mgmt = format!("http://{}", running.management_addr);
        self.running = Some(running);
    }

    pub fn store(&self) -> Arc<Store> {
        self.running.as_ref().unwrap().store.clone()
    }

    pub async fn stop(&mut self) {
        if let Some(r) = self.running.take() {
            r.shutdown().await;
        }
    }

    pub async fn restart(&mut self) {
        self.stop().await;
        self.boot().await;
    }

    pub fn data_dir(&self) -> PathBuf {
        self.dir.path().join("data")
    }

    pub fn client(&self, cred: (&str, &str)) -> Client {
        Client::new(&self.base, cred)
    }

    pub fn admin(&self) -> Client {
        self.client(ADMIN)
    }
}

/// Plain unsigned request (CORS preflight, anonymous probes).
pub async fn raw(method: &str, url: &str, headers: &[(&str, &str)], body: Vec<u8>) -> Resp {
    let client: hyper_util::client::legacy::Client<_, http_body_util::Full<bytes::Bytes>> =
        hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
            .build_http();
    let mut rb = http::Request::builder().method(method).uri(url);
    for (k, v) in headers {
        rb = rb.header(*k, *v);
    }
    let resp = client
        .request(
            rb.body(http_body_util::Full::new(bytes::Bytes::from(body)))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let body = http_body_util::BodyExt::collect(resp.into_body())
        .await
        .map(|b| b.to_bytes().to_vec())
        .unwrap_or_default();
    Resp {
        status,
        headers,
        body,
    }
}

pub fn now_amz() -> (String, String) {
    let t = time::OffsetDateTime::from(SystemTime::now());
    let date = format!("{:04}{:02}{:02}", t.year(), u8::from(t.month()), t.day());
    let amz = format!("{date}T{:02}{:02}{:02}Z", t.hour(), t.minute(), t.second());
    (date, amz)
}

pub fn amz_at(secs: i64) -> (String, String) {
    let t = time::OffsetDateTime::from_unix_timestamp(secs).unwrap();
    let date = format!("{:04}{:02}{:02}", t.year(), u8::from(t.month()), t.day());
    let amz = format!("{date}T{:02}{:02}{:02}Z", t.hour(), t.minute(), t.second());
    (date, amz)
}

pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// Encode a key the way AWS SDKs do: every byte except unreserved and `/`.
pub fn encode_key(key: &str) -> String {
    let mut out = String::new();
    for b in key.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub fn encode_q(v: &str) -> String {
    let mut out = String::new();
    for b in v.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub fn sha_hex(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}

#[derive(Clone, Debug)]
pub enum Payload {
    /// Signed payload with its SHA-256.
    Signed(Vec<u8>),
    Unsigned(Vec<u8>),
    /// aws-chunked signed chunks of the given size.
    StreamingSigned(Vec<u8>, usize),
    /// aws-chunked signed chunks with a signed checksum trailer.
    StreamingSignedTrailer(Vec<u8>, usize, String, String),
    /// aws-chunked unsigned chunks with a checksum trailer (name, value).
    StreamingUnsignedTrailer(Vec<u8>, usize, String, String),
}

pub struct Resp {
    pub status: u16,
    pub headers: http::HeaderMap,
    pub body: Vec<u8>,
}

impl Resp {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn header(&self, name: &str) -> Option<String> {
        self.headers
            .get(name)
            .map(|v| v.to_str().unwrap_or("").to_string())
    }

    pub fn code(&self) -> String {
        let t = self.text();
        t.split("<Code>")
            .nth(1)
            .and_then(|r| r.split("</Code>").next())
            .unwrap_or("")
            .to_string()
    }

    /// All text values of `<tag>` elements.
    pub fn all(&self, tag: &str) -> Vec<String> {
        let t = self.text();
        let open = format!("<{tag}>");
        let close = format!("</{tag}>");
        t.split(&open)
            .skip(1)
            .filter_map(|r| r.split(&close).next())
            .map(|s| {
                s.replace("&amp;", "&")
                    .replace("&lt;", "<")
                    .replace("&gt;", ">")
                    .replace("&quot;", "\"")
                    .replace("&apos;", "'")
            })
            .collect()
    }

    pub fn one(&self, tag: &str) -> String {
        self.all(tag).into_iter().next().unwrap_or_default()
    }
}

#[derive(Clone)]
pub struct Client {
    pub base: String,
    pub host: String,
    pub id: String,
    pub secret: String,
    pub http: hyper_util::client::legacy::Client<
        hyper_util::client::legacy::connect::HttpConnector,
        http_body_util::Full<bytes::Bytes>,
    >,
    pub region: String,
}

impl Client {
    pub fn new(base: &str, cred: (&str, &str)) -> Self {
        Self {
            base: base.to_string(),
            host: base.trim_start_matches("http://").to_string(),
            id: cred.0.to_string(),
            secret: cred.1.to_string(),
            http: hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                .build_http(),
            region: REGION.to_string(),
        }
    }

    fn canonical_query(query: &str) -> String {
        let mut pairs: Vec<(String, String)> = query
            .split('&')
            .filter(|s| !s.is_empty())
            .map(|p| {
                let (k, v) = p.split_once('=').unwrap_or((p, ""));
                (k.to_string(), v.to_string())
            })
            .collect();
        pairs.sort();
        pairs
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&")
    }

    /// Sign and send. `path` is already encoded; `query` is an encoded query.
    pub async fn send(
        &self,
        method: &str,
        path: &str,
        query: &str,
        headers: &[(&str, &str)],
        payload: Payload,
    ) -> Resp {
        let (date, amz) = now_amz();
        self.send_at(method, path, query, headers, payload, &date, &amz)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn send_at(
        &self,
        method: &str,
        path: &str,
        query: &str,
        headers: &[(&str, &str)],
        payload: Payload,
        date: &str,
        amz: &str,
    ) -> Resp {
        let scope = format!("{date}/{}/s3/aws4_request", self.region);
        let mut h: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| (k.to_lowercase(), v.to_string()))
            .collect();
        h.push(("host".into(), self.host.clone()));
        h.push(("x-amz-date".into(), amz.to_string()));
        let (content_sha, body_len, decoded_len) = match &payload {
            Payload::Signed(b) => (sha_hex(b), b.len(), None),
            Payload::Unsigned(b) => ("UNSIGNED-PAYLOAD".to_string(), b.len(), None),
            Payload::StreamingSigned(b, c) => (
                "STREAMING-AWS4-HMAC-SHA256-PAYLOAD".to_string(),
                chunked_len(b.len(), *c, true, None),
                Some(b.len()),
            ),
            Payload::StreamingSignedTrailer(b, c, n, v) => (
                "STREAMING-AWS4-HMAC-SHA256-PAYLOAD-TRAILER".to_string(),
                chunked_len(b.len(), *c, true, Some((n, v, true))),
                Some(b.len()),
            ),
            Payload::StreamingUnsignedTrailer(b, c, n, v) => (
                "STREAMING-UNSIGNED-PAYLOAD-TRAILER".to_string(),
                chunked_len(b.len(), *c, false, Some((n, v, false))),
                Some(b.len()),
            ),
        };
        let content_sha = match h.iter().find(|(k, _)| k == "x-amz-content-sha256") {
            Some((_, v)) => v.clone(),
            None => {
                h.push(("x-amz-content-sha256".into(), content_sha.clone()));
                content_sha
            }
        };
        if let Some(d) = decoded_len {
            h.push(("x-amz-decoded-content-length".into(), d.to_string()));
            h.push(("content-encoding".into(), "aws-chunked".into()));
            if let Payload::StreamingSignedTrailer(_, _, n, _)
            | Payload::StreamingUnsignedTrailer(_, _, n, _) = &payload
            {
                h.push(("x-amz-trailer".into(), n.clone()));
            }
        }
        let _ = body_len;
        h.sort();
        let signed: Vec<&str> = h.iter().map(|(k, _)| k.as_str()).collect();
        let mut signed_dedup = signed.clone();
        signed_dedup.dedup();
        let block: String = signed_dedup
            .iter()
            .map(|k| {
                let vals: Vec<&str> = h
                    .iter()
                    .filter(|(n, _)| n == k)
                    .map(|(_, v)| v.trim())
                    .collect();
                format!("{k}:{}\n", vals.join(","))
            })
            .collect();
        let signed_str = signed_dedup.join(";");
        let cr = format!(
            "{method}\n{path}\n{}\n{block}\n{signed_str}\n{content_sha}",
            Self::canonical_query(query)
        );
        let sts = format!(
            "AWS4-HMAC-SHA256\n{amz}\n{scope}\n{}",
            sha_hex(cr.as_bytes())
        );
        let key = sigv4::signing_key(&self.secret, date, &self.region, "s3");
        let seed = hex::encode(sigv4::hmac(&key, sts.as_bytes()));
        let auth = format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_str}, Signature={seed}",
            self.id
        );
        let body = match &payload {
            Payload::Signed(b) | Payload::Unsigned(b) => b.clone(),
            Payload::StreamingSigned(b, c) => {
                encode_chunks(b, *c, Some((&key, amz, &scope, &seed)), None)
            }
            Payload::StreamingSignedTrailer(b, c, n, v) => {
                encode_chunks(b, *c, Some((&key, amz, &scope, &seed)), Some((n, v)))
            }
            Payload::StreamingUnsignedTrailer(b, c, n, v) => {
                encode_chunks(b, *c, None, Some((n, v)))
            }
        };
        let url = if query.is_empty() {
            format!("{}{path}", self.base)
        } else {
            format!("{}{path}?{query}", self.base)
        };
        let mut rb = http::Request::builder().method(method).uri(url);
        for (k, v) in &h {
            if k == "content-length" {
                continue;
            }
            rb = rb.header(k.as_str(), v.as_str());
        }
        rb = rb.header("authorization", auth);
        let req = rb
            .body(http_body_util::Full::new(bytes::Bytes::from(body)))
            .unwrap();
        let resp = match self.http.request(req).await {
            Ok(r) => r,
            // The server may reject early and close while the body is still
            // being sent; report that as status 0.
            Err(_) => {
                return Resp {
                    status: 0,
                    headers: http::HeaderMap::new(),
                    body: Vec::new(),
                };
            }
        };
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let body = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .map(|b| b.to_bytes().to_vec())
            .unwrap_or_default();
        Resp {
            status,
            headers,
            body,
        }
    }

    pub async fn get(&self, path: &str, query: &str) -> Resp {
        self.send("GET", path, query, &[], Payload::Signed(vec![]))
            .await
    }

    pub async fn put(&self, path: &str, body: &[u8]) -> Resp {
        self.send("PUT", path, "", &[], Payload::Signed(body.to_vec()))
            .await
    }

    pub async fn put_h(&self, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Resp {
        self.send("PUT", path, "", headers, Payload::Signed(body.to_vec()))
            .await
    }

    pub async fn head(&self, path: &str) -> Resp {
        self.send("HEAD", path, "", &[], Payload::Signed(vec![]))
            .await
    }

    pub async fn delete(&self, path: &str) -> Resp {
        self.send("DELETE", path, "", &[], Payload::Signed(vec![]))
            .await
    }

    pub async fn create_bucket(&self, name: &str) -> Resp {
        self.send("PUT", &format!("/{name}"), "", &[], Payload::Signed(vec![]))
            .await
    }

    /// Header-signed request head with UNSIGNED-PAYLOAD, for driving a slow
    /// body over a raw TCP stream. Returns the full HTTP/1.1 request head.
    pub fn signed_head_unsigned_payload(
        &self,
        method: &str,
        path: &str,
        content_length: usize,
    ) -> String {
        let (date, amz) = now_amz();
        let scope = format!("{date}/{}/s3/aws4_request", self.region);
        let block = format!(
            "host:{}\nx-amz-content-sha256:UNSIGNED-PAYLOAD\nx-amz-date:{amz}\n",
            self.host
        );
        let signed = "host;x-amz-content-sha256;x-amz-date";
        let cr = format!("{method}\n{path}\n\n{block}\n{signed}\nUNSIGNED-PAYLOAD");
        let sts = format!(
            "AWS4-HMAC-SHA256\n{amz}\n{scope}\n{}",
            sha_hex(cr.as_bytes())
        );
        let key = sigv4::signing_key(&self.secret, &date, &self.region, "s3");
        let sig = hex::encode(sigv4::hmac(&key, sts.as_bytes()));
        format!(
            "{method} {path} HTTP/1.1\r\nhost: {}\r\nx-amz-content-sha256: UNSIGNED-PAYLOAD\r\nx-amz-date: {amz}\r\ncontent-length: {content_length}\r\nauthorization: AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={sig}\r\n\r\n",
            self.host, self.id
        )
    }

    /// Presigned URL (query authentication) for `method` on `path`.
    pub fn presign(&self, method: &str, path: &str, expires: i64, at: Option<i64>) -> String {
        let (date, amz) = match at {
            Some(t) => amz_at(t),
            None => now_amz(),
        };
        let scope = format!("{date}/{}/s3/aws4_request", self.region);
        let query = format!(
            "X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential={}&X-Amz-Date={amz}&X-Amz-Expires={expires}&X-Amz-SignedHeaders=host",
            encode_q(&format!("{}/{scope}", self.id))
        );
        let cr = format!(
            "{method}\n{path}\n{}\nhost:{}\n\nhost\nUNSIGNED-PAYLOAD",
            Self::canonical_query(&query),
            self.host
        );
        let sts = format!(
            "AWS4-HMAC-SHA256\n{amz}\n{scope}\n{}",
            sha_hex(cr.as_bytes())
        );
        let key = sigv4::signing_key(&self.secret, &date, &self.region, "s3");
        let sig = hex::encode(sigv4::hmac(&key, sts.as_bytes()));
        format!("{}{path}?{query}&X-Amz-Signature={sig}", self.base)
    }
}

fn chunked_len(
    len: usize,
    chunk: usize,
    signed: bool,
    trailer: Option<(&String, &String, bool)>,
) -> usize {
    encode_chunks(
        &vec![0u8; len],
        chunk,
        signed.then_some((&[0u8; 32], "20000101T000000Z", "x", &"0".repeat(64))),
        trailer.map(|(n, v, _)| (n, v)),
    )
    .len()
}

/// Build an aws-chunked body. Signed when `sig` is provided.
pub fn encode_chunks(
    data: &[u8],
    chunk: usize,
    sig: Option<(&[u8; 32], &str, &str, &String)>,
    trailer: Option<(&String, &String)>,
) -> Vec<u8> {
    let mut out = Vec::new();
    let mut prev = sig.map(|s| s.3.clone()).unwrap_or_default();
    let sign = |prev: &str, d: &[u8]| -> String {
        let (key, amz, scope, _) = sig.unwrap();
        hex::encode(sigv4::hmac(
            key,
            sigv4::chunk_string_to_sign(amz, scope, prev, &sha_hex(d)).as_bytes(),
        ))
    };
    let mut pieces: Vec<&[u8]> = data.chunks(chunk.max(1)).collect();
    pieces.push(&[]);
    for p in pieces {
        if sig.is_some() {
            let s = sign(&prev, p);
            out.extend_from_slice(format!("{:x};chunk-signature={s}\r\n", p.len()).as_bytes());
            prev = s;
        } else {
            out.extend_from_slice(format!("{:x}\r\n", p.len()).as_bytes());
        }
        if !p.is_empty() {
            out.extend_from_slice(p);
            out.extend_from_slice(b"\r\n");
        }
    }
    match trailer {
        Some((n, v)) => {
            out.extend_from_slice(format!("{n}:{v}\r\n").as_bytes());
            if let Some((key, amz, scope, _)) = sig {
                let canon = format!("{n}:{v}\n");
                let s = hex::encode(sigv4::hmac(
                    key,
                    sigv4::trailer_string_to_sign(amz, scope, &prev, &sha_hex(canon.as_bytes()))
                        .as_bytes(),
                ));
                out.extend_from_slice(format!("x-amz-trailer-signature:{s}\r\n").as_bytes());
            }
            out.extend_from_slice(b"\r\n");
        }
        None => out.extend_from_slice(b"\r\n"),
    }
    out
}

/// Recursively list regular files under a directory (test inspection only).
pub fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(files_under(&p));
            } else {
                out.push(p);
            }
        }
    }
    out
}
