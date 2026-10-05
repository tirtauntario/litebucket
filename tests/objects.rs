//! Object storage behavior (PUT-*, GET-*, DEL-*, COPY-*, FS-01, BODY-*).

mod common;

use common::*;
use litebucket::checksums::{Algorithm, b64, md5_b64};

async fn setup() -> (TestServer, Client) {
    let s = TestServer::start().await;
    let c = s.admin();
    let r = c.create_bucket("docs").await;
    assert_eq!(r.status, 200, "{}", r.text());
    (s, c)
}

#[tokio::test(flavor = "multi_thread")]
async fn put_01_bytes_and_metadata_survive_restart() {
    let (mut s, c) = setup().await;
    let big: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
    for (key, body) in [
        ("empty", Vec::new()),
        ("small.txt", b"hello".to_vec()),
        ("big.bin", big.clone()),
    ] {
        let r = c
            .put_h(
                &format!("/docs/{key}"),
                &[
                    ("content-type", "text/x-test"),
                    ("x-amz-meta-color", "blue"),
                    (
                        "content-disposition",
                        "attachment; filename*=UTF-8''%C3%A9t%C3%A9.txt",
                    ),
                    ("cache-control", "max-age=60"),
                ],
                &body,
            )
            .await;
        assert_eq!(r.status, 200, "{}", r.text());
        assert_eq!(
            r.header("etag").unwrap(),
            format!("\"{}\"", hex::encode(md5::Md5::digest_bytes(&body)))
        );
    }
    s.restart().await;
    let c = s.admin();
    for (key, body) in [
        ("empty", Vec::new()),
        ("small.txt", b"hello".to_vec()),
        ("big.bin", big),
    ] {
        let r = c.get(&format!("/docs/{key}"), "").await;
        assert_eq!(r.status, 200);
        assert_eq!(r.body, body, "{key}");
        assert_eq!(r.header("content-type").unwrap(), "text/x-test");
        assert_eq!(r.header("x-amz-meta-color").unwrap(), "blue");
        assert_eq!(r.header("cache-control").unwrap(), "max-age=60");
        assert_eq!(
            r.header("content-disposition").unwrap(),
            "attachment; filename*=UTF-8''%C3%A9t%C3%A9.txt"
        );
    }
}

trait DigestBytes {
    fn digest_bytes(b: &[u8]) -> Vec<u8>;
}
impl DigestBytes for md5::Md5 {
    fn digest_bytes(b: &[u8]) -> Vec<u8> {
        use md5::Digest;
        md5::Md5::digest(b).to_vec()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn fs_01_object_files_are_two_level_sharded() {
    let (s, c) = setup().await;
    c.put("/docs/customers/123/invoice.pdf", b"pdf").await;
    let files = files_under(&s.data_dir().join("objects"));
    assert_eq!(files.len(), 1);
    let rel = files[0].strip_prefix(s.data_dir().join("objects")).unwrap();
    let parts: Vec<_> = rel
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    assert_eq!(parts.len(), 3);
    assert_eq!(parts[2].len(), 32);
    assert_eq!(parts[0], parts[2][0..2]);
    assert_eq!(parts[1], parts[2][2..4]);
    assert!(!files[0].to_string_lossy().contains("customers"));
    assert!(files_under(&s.data_dir().join("staging")).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn put_02_invalid_checksum_or_signature_never_replaces() {
    let (s, c) = setup().await;
    c.put("/docs/k", b"original").await;
    // Wrong Content-MD5.
    let r = c
        .put_h("/docs/k", &[("content-md5", &md5_b64(b"other"))], b"new")
        .await;
    assert_eq!((r.status, r.code().as_str()), (400, "BadDigest"));
    // Wrong explicit CRC32.
    let r = c
        .put_h(
            "/docs/k",
            &[("x-amz-checksum-crc32", &b64(&Algorithm::Crc32.hash(b"zzz")))],
            b"new",
        )
        .await;
    assert_eq!((r.status, r.code().as_str()), (400, "BadDigest"));
    // Payload hash mismatch: sign one body, send another.
    let r = c
        .send(
            "PUT",
            "/docs/k",
            "",
            &[("x-amz-content-sha256", &sha_hex(b"something else"))],
            Payload::Signed(b"new!".to_vec()),
        )
        .await;
    assert_eq!(r.code(), "XAmzContentSHA256Mismatch");
    // Bad secret.
    let bad = Client::new(
        &s.base,
        ("admin-key", "wrongwrongwrongwrongwrongwrongwrong"),
    );
    let r = bad.put("/docs/k", b"new").await;
    assert_eq!(
        (r.status, r.code().as_str()),
        (403, "SignatureDoesNotMatch")
    );
    // Wrong trailer checksum on an unsigned-trailer stream.
    let r = c
        .send(
            "PUT",
            "/docs/k",
            "",
            &[],
            Payload::StreamingUnsignedTrailer(
                b"new data".to_vec(),
                3,
                "x-amz-checksum-crc32".into(),
                b64(&Algorithm::Crc32.hash(b"no")),
            ),
        )
        .await;
    assert_eq!(r.code(), "BadDigest");
    assert_eq!(c.get("/docs/k", "").await.body, b"original");
    assert!(
        files_under(&s.data_dir().join("staging")).len() <= 4,
        "rejected staging files are tracked for GC"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn body_01_02_04_all_payload_modes_store_exact_bytes() {
    let (_s, c) = setup().await;
    let data: Vec<u8> = (0..200_000u32).map(|i| (i * 7 % 256) as u8).collect();
    let crc = |a: Algorithm| b64(&a.hash(&data));
    let cases = vec![
        ("signed", Payload::Signed(data.clone())),
        ("unsigned", Payload::Unsigned(data.clone())),
        ("chunked", Payload::StreamingSigned(data.clone(), 65_536)),
        (
            "chunked-trailer",
            Payload::StreamingSignedTrailer(
                data.clone(),
                65_536,
                "x-amz-checksum-crc32c".into(),
                crc(Algorithm::Crc32c),
            ),
        ),
        (
            "unsigned-trailer",
            Payload::StreamingUnsignedTrailer(
                data.clone(),
                16_384,
                "x-amz-checksum-crc64nvme".into(),
                crc(Algorithm::Crc64Nvme),
            ),
        ),
    ];
    for (name, p) in cases {
        let r = c.send("PUT", &format!("/docs/{name}"), "", &[], p).await;
        assert_eq!(r.status, 200, "{name}: {}", r.text());
        let g = c.get(&format!("/docs/{name}"), "").await;
        assert_eq!(g.body, data, "{name}");
        assert_eq!(
            g.header("content-encoding"),
            None,
            "aws-chunked is not stored"
        );
    }
    for a in Algorithm::ALL {
        let r = c
            .put_h("/docs/explicit", &[(a.header_name(), &crc(a))], &data)
            .await;
        assert_eq!(r.status, 200, "{a:?} {}", r.text());
        assert_eq!(r.header(a.header_name()).unwrap(), crc(a));
        let h = c
            .send(
                "HEAD",
                "/docs/explicit",
                "",
                &[("x-amz-checksum-mode", "ENABLED")],
                Payload::Signed(vec![]),
            )
            .await;
        assert_eq!(h.header(a.header_name()).unwrap(), crc(a));
        assert_eq!(h.header("x-amz-checksum-type").unwrap(), "FULL_OBJECT");
    }
    // Default CRC64NVME is computed when nothing is requested.
    c.put("/docs/default", &data).await;
    let h = c
        .send(
            "GET",
            "/docs/default",
            "",
            &[("x-amz-checksum-mode", "ENABLED")],
            Payload::Signed(vec![]),
        )
        .await;
    assert_eq!(
        h.header("x-amz-checksum-crc64nvme").unwrap(),
        crc(Algorithm::Crc64Nvme)
    );
    // Ranged GET omits whole-object checksums.
    let r = c
        .send(
            "GET",
            "/docs/default",
            "",
            &[("x-amz-checksum-mode", "ENABLED"), ("range", "bytes=0-9")],
            Payload::Signed(vec![]),
        )
        .await;
    assert_eq!(r.status, 206);
    assert!(r.header("x-amz-checksum-crc64nvme").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn body_03_bad_chunk_signature_or_truncation_never_publishes() {
    let (_s, c) = setup().await;
    c.put("/docs/k", b"original").await;
    // Declared decoded length larger than the data.
    let r = c
        .send(
            "PUT",
            "/docs/k",
            "",
            &[("x-amz-decoded-content-length", "999")],
            Payload::StreamingSigned(b"short".to_vec(), 2),
        )
        .await;
    assert!(r.status >= 400, "{}", r.text());
    // Undeclared trailer name.
    let r = c
        .send(
            "PUT",
            "/docs/k",
            "",
            &[("x-amz-trailer", "x-amz-checksum-sha1")],
            Payload::StreamingUnsignedTrailer(
                b"data".to_vec(),
                2,
                "x-amz-checksum-crc32".into(),
                b64(&Algorithm::Crc32.hash(b"data")),
            ),
        )
        .await;
    assert!(r.status >= 400);
    assert_eq!(c.get("/docs/k", "").await.body, b"original");
}

#[tokio::test(flavor = "multi_thread")]
async fn put_03_04_conditional_writes() {
    let (_s, c) = setup().await;
    let r = c.put_h("/docs/k", &[("if-match", "\"abc\"")], b"x").await;
    assert_eq!((r.status, r.code().as_str()), (404, "NoSuchKey"));
    assert_eq!(
        c.put_h("/docs/k", &[("if-none-match", "*")], b"v1")
            .await
            .status,
        200
    );
    let r = c.put_h("/docs/k", &[("if-none-match", "*")], b"v2").await;
    assert_eq!((r.status, r.code().as_str()), (412, "PreconditionFailed"));
    let etag = c.head("/docs/k").await.header("etag").unwrap();
    assert_eq!(
        c.put_h("/docs/k", &[("if-match", &etag)], b"v3")
            .await
            .status,
        200
    );
    assert_eq!(
        c.put_h("/docs/k", &[("if-match", &etag)], b"v4")
            .await
            .status,
        412
    );
    assert_eq!(c.get("/docs/k", "").await.body, b"v3");
    let r = c
        .put_h("/docs/k", &[("if-none-match", "\"x\"")], b"v5")
        .await;
    assert_eq!(r.code(), "NotImplemented");
}

#[tokio::test(flavor = "multi_thread")]
async fn put_03_concurrent_create_has_one_winner() {
    let (_s, c) = setup().await;
    let mut tasks = Vec::new();
    for i in 0..12 {
        let c = c.clone();
        tasks.push(tokio::spawn(async move {
            c.put_h(
                "/docs/race",
                &[("if-none-match", "*")],
                format!("writer {i}").as_bytes(),
            )
            .await
            .status
        }));
    }
    let mut ok = 0;
    for t in tasks {
        match t.await.unwrap() {
            200 => ok += 1,
            412 => {}
            other => panic!("unexpected status {other}"),
        }
    }
    assert_eq!(ok, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn get_02_ranges_and_conditional_reads() {
    let (_s, c) = setup().await;
    c.put("/docs/r", b"0123456789").await;
    let g = |range: &'static str| {
        let c = c.clone();
        async move {
            c.send(
                "GET",
                "/docs/r",
                "",
                &[("range", range)],
                Payload::Signed(vec![]),
            )
            .await
        }
    };
    let r = g("bytes=2-4").await;
    assert_eq!((r.status, r.body.as_slice()), (206, &b"234"[..]));
    assert_eq!(r.header("content-range").unwrap(), "bytes 2-4/10");
    assert_eq!(g("bytes=7-").await.body, b"789");
    assert_eq!(g("bytes=-3").await.body, b"789");
    assert_eq!(g("bytes=5-100").await.body, b"56789");
    let r = g("bytes=10-").await;
    assert_eq!((r.status, r.code().as_str()), (416, "InvalidRange"));
    assert_eq!(r.header("content-range").unwrap(), "bytes */10");
    assert_eq!(g("bytes=0-1,4-5").await.status, 400);
    assert_eq!(
        g("bytes=abc").await.status,
        200,
        "malformed range is ignored"
    );
    let h = c
        .send(
            "HEAD",
            "/docs/r",
            "",
            &[("range", "bytes=0-3")],
            Payload::Signed(vec![]),
        )
        .await;
    assert_eq!(h.status, 206);
    assert_eq!(h.header("content-length").unwrap(), "4");
    assert!(h.body.is_empty());

    let etag = c.head("/docs/r").await.header("etag").unwrap();
    let lm = c.head("/docs/r").await.header("last-modified").unwrap();
    let cond = |h: Vec<(&'static str, String)>| {
        let c = c.clone();
        async move {
            let hs: Vec<(&str, &str)> = h.iter().map(|(k, v)| (*k, v.as_str())).collect();
            c.send("GET", "/docs/r", "", &hs, Payload::Signed(vec![]))
                .await
                .status
        }
    };
    assert_eq!(cond(vec![("if-match", etag.clone())]).await, 200);
    assert_eq!(cond(vec![("if-match", "\"nope\"".into())]).await, 412);
    assert_eq!(cond(vec![("if-none-match", etag.clone())]).await, 304);
    assert_eq!(cond(vec![("if-modified-since", lm.clone())]).await, 304);
    assert_eq!(
        cond(vec![(
            "if-unmodified-since",
            "Mon, 01 Jan 2001 00:00:00 GMT".into()
        )])
        .await,
        412
    );
    // If-Match true overrides If-Unmodified-Since false.
    assert_eq!(
        cond(vec![
            ("if-match", etag.clone()),
            (
                "if-unmodified-since",
                "Mon, 01 Jan 2001 00:00:00 GMT".into()
            )
        ])
        .await,
        200
    );
    // HEAD errors have no body.
    let h = c
        .send("HEAD", "/docs/missing", "", &[], Payload::Signed(vec![]))
        .await;
    assert_eq!(h.status, 404);
    assert!(h.body.is_empty());
    // Response overrides.
    let r = c
        .get(
            "/docs/r",
            "response-content-type=application%2Fpdf&response-content-disposition=inline",
        )
        .await;
    assert_eq!(r.header("content-type").unwrap(), "application/pdf");
    assert_eq!(r.header("content-disposition").unwrap(), "inline");
}

#[tokio::test(flavor = "multi_thread")]
async fn get_01_reads_one_generation_during_overwrites() {
    let (_s, c) = setup().await;
    let a = vec![b'a'; 2_000_000];
    let b = vec![b'b'; 1_000_000];
    c.put("/docs/g", &a).await;
    let writer = {
        let (c, a, b) = (c.clone(), a.clone(), b.clone());
        tokio::spawn(async move {
            for i in 0..20 {
                c.put("/docs/g", if i % 2 == 0 { &b } else { &a }).await;
            }
            c.delete("/docs/g").await;
        })
    };
    let mut seen = 0;
    for _ in 0..40 {
        let r = c.get("/docs/g", "").await;
        if r.status == 404 {
            continue;
        }
        assert_eq!(r.status, 200);
        let len: usize = r.header("content-length").unwrap().parse().unwrap();
        assert_eq!(r.body.len(), len);
        let first = r.body[0];
        assert!(r.body.iter().all(|x| *x == first), "mixed generations");
        let expect_etag = if first == b'a' { &a } else { &b };
        use md5::Digest;
        assert_eq!(
            r.header("etag").unwrap(),
            format!("\"{}\"", hex::encode(md5::Md5::digest(expect_etag)))
        );
        seen += 1;
    }
    writer.await.unwrap();
    assert!(seen > 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn del_01_delete_is_idempotent_and_hides_immediately() {
    let (s, c) = setup().await;
    c.put("/docs/d", b"x").await;
    assert_eq!(c.delete("/docs/d").await.status, 204);
    assert_eq!(c.delete("/docs/d").await.status, 204);
    assert_eq!(c.get("/docs/d", "").await.status, 404);
    assert_eq!(c.delete("/nobucket/d").await.code(), "NoSuchBucket");
    // Physical deletion is deferred to the collector.
    let store = s.store();
    let (blobs, _, _) = {
        litebucket::maintenance::refresh_gauges(&store)
            .await
            .unwrap();
        store.maintenance_gauges()
    };
    assert_eq!(blobs, 1, "one garbage blob awaiting collection");
}

#[tokio::test(flavor = "multi_thread")]
async fn del_02_delete_objects_validates_then_reports_per_key() {
    let (s, c) = setup().await;
    for k in ["a", "b", "customers/123/x", "customers/999/y"] {
        c.put(&format!("/docs/{k}"), b"x").await;
    }
    let xml = "<Delete><Object><Key>a</Key></Object><Object><Key>missing</Key></Object></Delete>";
    // Missing integrity header: rejected before any deletion.
    let r = c
        .send(
            "POST",
            "/docs",
            "delete",
            &[],
            Payload::Signed(xml.as_bytes().to_vec()),
        )
        .await;
    assert_eq!(r.status, 400);
    assert_eq!(c.get("/docs/a", "").await.status, 200);
    // Bad Content-MD5: rejected entirely.
    let r = c
        .send(
            "POST",
            "/docs",
            "delete",
            &[("content-md5", &md5_b64(b"x"))],
            Payload::Signed(xml.as_bytes().to_vec()),
        )
        .await;
    assert_eq!(r.code(), "BadDigest");
    assert_eq!(c.get("/docs/a", "").await.status, 200);
    let r = c
        .send(
            "POST",
            "/docs",
            "delete",
            &[("content-md5", &md5_b64(xml.as_bytes()))],
            Payload::Signed(xml.as_bytes().to_vec()),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.all("Key"), vec!["a", "missing"]);
    assert_eq!(c.get("/docs/a", "").await.status, 404);
    // Per-key authorization for a prefix-scoped credential (no delete grant).
    let reader = s.client(READER);
    let xml = "<Delete><Quiet>true</Quiet><Object><Key>customers/123/x</Key></Object><Object><Key>b</Key></Object></Delete>";
    let r = reader
        .send(
            "POST",
            "/docs",
            "delete",
            &[("content-md5", &md5_b64(xml.as_bytes()))],
            Payload::Signed(xml.as_bytes().to_vec()),
        )
        .await;
    assert_eq!(r.status, 200);
    assert_eq!(r.all("Code"), vec!["AccessDenied", "AccessDenied"]);
    assert_eq!(c.get("/docs/b", "").await.status, 200);
    // Malformed XML: nothing deleted.
    let bad = "<Delete><Object><Key>b</Key></Object>";
    let r = c
        .send(
            "POST",
            "/docs",
            "delete",
            &[("content-md5", &md5_b64(bad.as_bytes()))],
            Payload::Signed(bad.as_bytes().to_vec()),
        )
        .await;
    assert_eq!(r.code(), "MalformedXML");
    assert_eq!(c.get("/docs/b", "").await.status, 200);
}

#[tokio::test(flavor = "multi_thread")]
async fn copy_01_02_copy_semantics_and_isolation() {
    let (s, c) = setup().await;
    c.create_bucket("other").await;
    c.put_h(
        "/docs/src",
        &[("content-type", "text/a"), ("x-amz-meta-k", "v")],
        b"source bytes",
    )
    .await;
    let r = c
        .put_h("/other/dst", &[("x-amz-copy-source", "/docs/src")], b"")
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert!(r.text().contains("<CopyObjectResult"));
    let g = c.get("/other/dst", "").await;
    assert_eq!(g.body, b"source bytes");
    assert_eq!(g.header("content-type").unwrap(), "text/a");
    assert_eq!(g.header("x-amz-meta-k").unwrap(), "v");
    // REPLACE directive.
    c.put_h(
        "/other/dst2",
        &[
            ("x-amz-copy-source", "docs/src"),
            ("x-amz-metadata-directive", "REPLACE"),
            ("content-type", "text/b"),
        ],
        b"",
    )
    .await;
    let g = c.get("/other/dst2", "").await;
    assert_eq!(g.header("content-type").unwrap(), "text/b");
    assert!(g.header("x-amz-meta-k").is_none());
    // Overwriting the source does not affect the copy (separate files).
    c.put("/docs/src", b"changed").await;
    assert_eq!(c.get("/other/dst", "").await.body, b"source bytes");
    // Self-copy without changes is illegal; with REPLACE it works.
    let r = c
        .put_h("/docs/src", &[("x-amz-copy-source", "/docs/src")], b"")
        .await;
    assert_eq!(r.code(), "InvalidRequest");
    let r = c
        .put_h(
            "/docs/src",
            &[
                ("x-amz-copy-source", "/docs/src"),
                ("x-amz-metadata-directive", "REPLACE"),
                ("x-amz-meta-n", "1"),
            ],
            b"",
        )
        .await;
    assert_eq!(r.status, 200);
    assert_eq!(
        c.get("/docs/src", "").await.header("x-amz-meta-n").unwrap(),
        "1"
    );
    // Source conditions.
    let r = c
        .put_h(
            "/other/x",
            &[
                ("x-amz-copy-source", "/docs/src"),
                ("x-amz-copy-source-if-match", "\"nope\""),
            ],
            b"",
        )
        .await;
    assert_eq!(r.status, 412);
    // Missing source.
    let r = c
        .put_h("/other/x", &[("x-amz-copy-source", "/docs/none")], b"")
        .await;
    assert_eq!(r.code(), "NoSuchKey");
    // Source read and destination write are authorized independently.
    let reader = s.client(READER);
    c.put("/docs/customers/123/doc", b"mine").await;
    let r = reader
        .put_h(
            "/docs/customers/123/copy",
            &[("x-amz-copy-source", "/docs/customers/123/doc")],
            b"",
        )
        .await;
    assert_eq!(r.code(), "AccessDenied", "no write grant");
    let app = s.client(APP);
    let r = app
        .put_h("/docs/stolen", &[("x-amz-copy-source", "/other/dst")], b"")
        .await;
    assert_eq!(r.code(), "AccessDenied", "no read grant on source bucket");
    // Copies have their own files.
    let objects = files_under(&s.data_dir().join("objects")).len();
    assert!(objects >= 4);
}

#[tokio::test(flavor = "multi_thread")]
async fn key_bytes_are_preserved_exactly() {
    let (_s, c) = setup().await;
    let keys = [
        "a//b",
        "dot/./seg/../x",
        "trailing/",
        "sp ace+plus%pct",
        "unicode/é/e\u{301}/日本",
        "tab\there",
        "~tilde!*'()",
    ];
    for k in keys {
        let r = c
            .put(&format!("/docs/{}", encode_key(k)), k.as_bytes())
            .await;
        assert_eq!(r.status, 200, "{k}: {}", r.text());
    }
    for k in keys {
        assert_eq!(
            c.get(&format!("/docs/{}", encode_key(k)), "").await.body,
            k.as_bytes(),
            "{k}"
        );
    }
    let l = c.get("/docs", "list-type=2&encoding-type=url").await;
    let mut want: Vec<String> = keys
        .iter()
        .map(|k| litebucket::s3::listing::url_encode(k.as_bytes()))
        .collect();
    want.sort();
    let mut got = l.all("Key");
    got.sort();
    assert_eq!(got, want);
    // NUL and other XML-forbidden characters are rejected.
    let r = c.put("/docs/bad%00key", b"x").await;
    assert_eq!(r.status, 400);
}

#[tokio::test(flavor = "multi_thread")]
async fn put_02_oversized_and_short_bodies_preserve_old_object() {
    let s = TestServer::start_with("[limits]\nmax_single_put_bytes = 1000\nmax_object_bytes = 107374182400\nmax_part_bytes = 5368709120\n").await;
    let c = s.admin();
    c.create_bucket("docs").await;
    c.put("/docs/k", b"keep").await;
    let r = c.put("/docs/k", &vec![0u8; 2000]).await;
    assert_eq!(r.code(), "EntityTooLarge");
    assert_eq!(c.get("/docs/k", "").await.body, b"keep");
}
