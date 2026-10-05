//! Multipart upload state machine (MPU-01..08, CAP-03).

mod common;

use common::*;
use litebucket::checksums::{self, Algorithm, b64};

const MIB: usize = 1024 * 1024;

async fn setup() -> (TestServer, Client) {
    let s = TestServer::start().await;
    let c = s.admin();
    assert_eq!(c.create_bucket("docs").await.status, 200);
    (s, c)
}

async fn initiate(c: &Client, key: &str, headers: &[(&str, &str)]) -> String {
    let r = c
        .send(
            "POST",
            &format!("/docs/{key}"),
            "uploads",
            headers,
            Payload::Signed(vec![]),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    r.one("UploadId")
}

async fn part(
    c: &Client,
    key: &str,
    id: &str,
    n: u32,
    data: &[u8],
    headers: &[(&str, &str)],
) -> Resp {
    c.send(
        "PUT",
        &format!("/docs/{key}"),
        &format!("partNumber={n}&uploadId={id}"),
        headers,
        Payload::Signed(data.to_vec()),
    )
    .await
}

fn manifest(parts: &[(u32, String)]) -> Vec<u8> {
    let mut x = String::from("<CompleteMultipartUpload>");
    for (n, e) in parts {
        x.push_str(&format!(
            "<Part><PartNumber>{n}</PartNumber><ETag>{e}</ETag></Part>"
        ));
    }
    x.push_str("</CompleteMultipartUpload>");
    x.into_bytes()
}

async fn complete(
    c: &Client,
    key: &str,
    id: &str,
    body: Vec<u8>,
    headers: &[(&str, &str)],
) -> Resp {
    c.send(
        "POST",
        &format!("/docs/{key}"),
        &format!("uploadId={id}"),
        headers,
        Payload::Signed(body),
    )
    .await
}

fn data(n: usize, seed: u8) -> Vec<u8> {
    (0..n)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn mpu_01_parallel_parts_and_replacement() {
    let (s, c) = setup().await;
    let id = initiate(&c, "big", &[]).await;
    let p1 = data(5 * MIB, 1);
    let p2 = data(5 * MIB, 2);
    let p3 = data(1000, 3);
    // Parallel uploads of different parts.
    let mut tasks = Vec::new();
    for (n, d) in [(1u32, p1.clone()), (2, p2.clone()), (3, data(10, 9))] {
        let (c, id) = (c.clone(), id.clone());
        tasks.push(tokio::spawn(async move {
            (n, part(&c, "big", &id, n, &d, &[]).await)
        }));
    }
    let mut etags = std::collections::BTreeMap::new();
    for t in tasks {
        let (n, r) = t.await.unwrap();
        assert_eq!(r.status, 200, "{}", r.text());
        etags.insert(n, r.header("etag").unwrap());
    }
    // Replace part 3; the old mapping is released.
    let r = part(&c, "big", &id, 3, &p3, &[]).await;
    etags.insert(3, r.header("etag").unwrap());
    let l = c.get("/docs/big", &format!("uploadId={id}")).await;
    assert_eq!(l.all("PartNumber"), vec!["1", "2", "3"]);
    assert_eq!(
        l.all("Size"),
        vec![(5 * MIB).to_string(), (5 * MIB).to_string(), "1000".into()]
    );
    let parts: Vec<(u32, String)> = etags.into_iter().collect();
    let r = complete(&c, "big", &id, manifest(&parts), &[]).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let mut whole = p1.clone();
    whole.extend_from_slice(&p2);
    whole.extend_from_slice(&p3);
    let g = c.get("/docs/big", "").await;
    assert_eq!(g.body.len(), whole.len());
    assert!(g.body == whole);
    use md5::Digest;
    let md5s: Vec<[u8; 16]> = [&p1, &p2, &p3]
        .iter()
        .map(|p| md5::Md5::digest(p).into())
        .collect();
    let expected = checksums::multipart_etag(&md5s);
    assert_eq!(g.header("etag").unwrap(), format!("\"{expected}\""));
    assert!(
        g.header("x-amz-mp-parts-count").is_none(),
        "only with partNumber"
    );
    // partNumber reads return each part's exact byte range.
    for (n, p) in [(1u32, &p1), (2, &p2), (3, &p3)] {
        let r = c.get("/docs/big", &format!("partNumber={n}")).await;
        assert_eq!(r.status, 206);
        assert_eq!(r.header("x-amz-mp-parts-count").unwrap(), "3");
        assert!(r.body == *p, "part {n}");
    }
    assert_eq!(
        c.get("/docs/big", "partNumber=4").await.code(),
        "InvalidPartNumber"
    );
    let h = c
        .send(
            "HEAD",
            "/docs/big",
            "partNumber=1",
            &[],
            Payload::Signed(vec![]),
        )
        .await;
    assert_eq!(h.header("content-length").unwrap(), (5 * MIB).to_string());
    // Parts go to multipart/aa/bb/id; after completion they become garbage.
    assert!(files_under(&s.data_dir().join("staging")).is_empty());
    let store = s.store();
    litebucket::maintenance::refresh_gauges(&store)
        .await
        .unwrap();
    assert_eq!(
        store.maintenance_gauges().0,
        4,
        "3 completed parts + replaced part 3"
    );
    assert_eq!(store.maintenance_gauges().2, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn mpu_02_parts_survive_restart_and_listings_paginate() {
    let (mut s, c) = setup().await;
    let id = initiate(&c, "a/one", &[]).await;
    for n in 1..=5u32 {
        part(&c, "a/one", &id, n, &data(100, n as u8), &[]).await;
    }
    let id2 = initiate(&c, "a/two", &[]).await;
    let id3 = initiate(&c, "b/three", &[]).await;
    let id4 = initiate(&c, "a/one", &[]).await;
    s.restart().await;
    let c = s.admin();
    let mut seen = Vec::new();
    let mut marker = 0;
    loop {
        let r = c
            .get(
                "/docs/a/one",
                &format!("uploadId={id}&max-parts=2&part-number-marker={marker}"),
            )
            .await;
        assert_eq!(r.status, 200, "{}", r.text());
        seen.extend(r.all("PartNumber"));
        if r.one("IsTruncated") != "true" {
            break;
        }
        marker = r.one("NextPartNumberMarker").parse().unwrap();
    }
    assert_eq!(seen, vec!["1", "2", "3", "4", "5"]);
    // ListMultipartUploads with paging over (key, upload id).
    let mut uploads = Vec::new();
    let mut q = "uploads&max-uploads=1".to_string();
    loop {
        let r = c.get("/docs", &q).await;
        assert_eq!(r.status, 200, "{}", r.text());
        uploads.extend(r.all("UploadId"));
        if r.one("IsTruncated") != "true" {
            break;
        }
        q = format!(
            "uploads&max-uploads=1&key-marker={}&upload-id-marker={}",
            encode_q(&r.one("NextKeyMarker")),
            r.one("NextUploadIdMarker")
        );
    }
    let mut want = vec![id.clone(), id2.clone(), id3.clone(), id4.clone()];
    want.sort();
    let mut got = uploads.clone();
    got.sort();
    assert_eq!(got, want);
    let r = c.get("/docs", "uploads&delimiter=%2F").await;
    assert_eq!(
        r.all("Prefix").iter().filter(|p| p.ends_with('/')).count(),
        2
    );
    let r = c.get("/docs", "uploads&prefix=b%2F").await;
    assert_eq!(r.all("UploadId"), vec![id3]);
    // Resumed upload completes after restart.
    let l = c.get("/docs/a/one", &format!("uploadId={id}")).await;
    let parts: Vec<(u32, String)> = l
        .all("PartNumber")
        .iter()
        .zip(l.all("ETag"))
        .map(|(n, e)| (n.parse().unwrap(), e))
        .take(1)
        .collect();
    let r = complete(&c, "a/one", &id, manifest(&parts), &[]).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(c.get("/docs/a/one", "").await.body, data(100, 1));
}

#[tokio::test(flavor = "multi_thread")]
async fn mpu_03_invalid_manifests_cannot_complete() {
    let (_s, c) = setup().await;
    let id = initiate(&c, "m", &[]).await;
    let e1 = part(&c, "m", &id, 1, &data(1000, 1), &[])
        .await
        .header("etag")
        .unwrap();
    let e2 = part(&c, "m", &id, 2, &data(1000, 2), &[])
        .await
        .header("etag")
        .unwrap();
    let cases = [
        (
            manifest(&[(2, e2.clone()), (1, e1.clone())]),
            "InvalidPartOrder",
        ),
        (
            manifest(&[(1, e1.clone()), (1, e1.clone())]),
            "InvalidPartOrder",
        ),
        (manifest(&[(1, e1.clone()), (3, e2.clone())]), "InvalidPart"),
        (manifest(&[(1, "\"0000\"".into())]), "InvalidPart"),
        (
            manifest(&[(1, e1.clone()), (2, e2.clone())]),
            "EntityTooSmall",
        ),
        (
            b"<CompleteMultipartUpload></CompleteMultipartUpload>".to_vec(),
            "MalformedXML",
        ),
        (b"<CompleteMultipartUpload><Part>".to_vec(), "MalformedXML"),
    ];
    for (body, code) in cases {
        let r = complete(&c, "m", &id, body, &[]).await;
        assert_eq!(r.code(), code);
    }
    assert_eq!(c.head("/docs/m").await.status, 404);
    // Still OPEN: the single small final part completes.
    let r = complete(&c, "m", &id, manifest(&[(2, e2.clone())]), &[]).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(c.get("/docs/m", "").await.body, data(1000, 2));
    // Invalid part numbers.
    let id = initiate(&c, "m2", &[]).await;
    assert_eq!(
        part(&c, "m2", &id, 0, b"x", &[]).await.code(),
        "InvalidArgument"
    );
    assert_eq!(
        part(&c, "m2", &id, 10_001, b"x", &[]).await.code(),
        "InvalidArgument"
    );
    // Unknown and malformed upload IDs.
    assert_eq!(
        part(&c, "m2", "0123456789abcdef0123456789abcdef", 1, b"x", &[])
            .await
            .code(),
        "NoSuchUpload"
    );
    assert_eq!(
        part(&c, "m2", &encode_q("../../x"), 1, b"x", &[])
            .await
            .code(),
        "NoSuchUpload"
    );
    // An upload ID is bound to its key.
    assert_eq!(
        part(&c, "other", &id, 1, b"x", &[]).await.code(),
        "NoSuchUpload"
    );
    // Bad part checksum.
    let r = part(
        &c,
        "m2",
        &id,
        1,
        b"abc",
        &[("x-amz-checksum-crc32", &b64(&Algorithm::Crc32.hash(b"abd")))],
    )
    .await;
    assert_eq!(r.code(), "BadDigest");
}

#[tokio::test(flavor = "multi_thread")]
async fn mpu_04_terminal_uploads_cannot_be_revived() {
    let (_s, c) = setup().await;
    let id = initiate(&c, "t", &[]).await;
    let e = part(&c, "t", &id, 1, b"one", &[])
        .await
        .header("etag")
        .unwrap();
    assert_eq!(
        c.send(
            "DELETE",
            "/docs/t",
            &format!("uploadId={id}"),
            &[],
            Payload::Signed(vec![])
        )
        .await
        .status,
        204
    );
    assert_eq!(
        part(&c, "t", &id, 2, b"two", &[]).await.code(),
        "NoSuchUpload"
    );
    assert_eq!(
        complete(&c, "t", &id, manifest(&[(1, e)]), &[])
            .await
            .code(),
        "NoSuchUpload"
    );
    assert_eq!(
        c.send(
            "DELETE",
            "/docs/t",
            &format!("uploadId={id}"),
            &[],
            Payload::Signed(vec![])
        )
        .await
        .code(),
        "NoSuchUpload"
    );
    assert_eq!(
        c.get("/docs/t", &format!("uploadId={id}")).await.code(),
        "NoSuchUpload"
    );
    // Race many part uploads against completion and abort.
    for round in 0..3 {
        let key = format!("race{round}");
        let id = initiate(&c, &key, &[]).await;
        let e = part(&c, &key, &id, 1, &data(100, 1), &[])
            .await
            .header("etag")
            .unwrap();
        let mut tasks = Vec::new();
        for n in 2..10u32 {
            let (c, id, key) = (c.clone(), id.clone(), key.clone());
            tasks.push(tokio::spawn(async move {
                part(&c, &key, &id, n, b"late", &[]).await.status
            }));
        }
        let finisher = {
            let (c, id, key) = (c.clone(), id.clone(), key.clone());
            tokio::spawn(async move {
                if round == 1 {
                    c.send(
                        "DELETE",
                        &format!("/docs/{key}"),
                        &format!("uploadId={id}"),
                        &[],
                        Payload::Signed(vec![]),
                    )
                    .await
                    .status
                } else {
                    complete(&c, &key, &id, manifest(&[(1, e)]), &[])
                        .await
                        .status
                }
            })
        };
        let fin = finisher.await.unwrap();
        for t in tasks {
            let st = t.await.unwrap();
            assert!(st == 200 || st == 404, "{st}");
        }
        if round == 1 {
            assert_eq!(fin, 204);
            assert_eq!(c.head(&format!("/docs/{key}")).await.status, 404);
        } else {
            assert_eq!(fin, 200);
            assert_eq!(
                c.get(&format!("/docs/{key}"), "").await.body,
                data(100, 1),
                "only selected parts"
            );
        }
        assert_eq!(
            part(&c, &key, &id, 11, b"x", &[]).await.code(),
            "NoSuchUpload"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn mpu_05_checksum_modes() {
    let (_s, c) = setup().await;
    let p1 = data(5 * MIB, 4);
    let p2 = data(777, 5);
    let mut whole = p1.clone();
    whole.extend_from_slice(&p2);
    // Default: CRC64NVME FULL_OBJECT computed over the assembled object.
    let id = initiate(&c, "d", &[]).await;
    let e1 = part(&c, "d", &id, 1, &p1, &[])
        .await
        .header("etag")
        .unwrap();
    let e2 = part(&c, "d", &id, 2, &p2, &[])
        .await
        .header("etag")
        .unwrap();
    assert_eq!(
        complete(&c, "d", &id, manifest(&[(1, e1), (2, e2)]), &[])
            .await
            .status,
        200
    );
    let h = c
        .send(
            "HEAD",
            "/docs/d",
            "",
            &[("x-amz-checksum-mode", "ENABLED")],
            Payload::Signed(vec![]),
        )
        .await;
    assert_eq!(
        h.header("x-amz-checksum-crc64nvme").unwrap(),
        b64(&Algorithm::Crc64Nvme.hash(&whole))
    );
    assert_eq!(h.header("x-amz-checksum-type").unwrap(), "FULL_OBJECT");

    // Explicit composite CRC32C.
    let id = initiate(&c, "comp", &[("x-amz-checksum-algorithm", "CRC32C")]).await;
    let c1 = b64(&Algorithm::Crc32c.hash(&p1));
    let c2 = b64(&Algorithm::Crc32c.hash(&p2));
    let r1 = part(&c, "comp", &id, 1, &p1, &[("x-amz-checksum-crc32c", &c1)]).await;
    assert_eq!(r1.header("x-amz-checksum-crc32c").unwrap(), c1);
    let r2 = part(&c, "comp", &id, 2, &p2, &[("x-amz-checksum-crc32c", &c2)]).await;
    // Wrong algorithm on a part of an explicit upload.
    assert_eq!(
        part(
            &c,
            "comp",
            &id,
            3,
            b"x",
            &[("x-amz-checksum-crc32", &b64(&Algorithm::Crc32.hash(b"x")))]
        )
        .await
        .code(),
        "InvalidRequest"
    );
    // Composite completion requires per-part checksums.
    let no_sums = manifest(&[
        (1, r1.header("etag").unwrap()),
        (2, r2.header("etag").unwrap()),
    ]);
    assert_eq!(
        complete(&c, "comp", &id, no_sums, &[]).await.code(),
        "InvalidRequest"
    );
    let body = format!(
        "<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>{}</ETag><ChecksumCRC32C>{c1}</ChecksumCRC32C></Part><Part><PartNumber>2</PartNumber><ETag>{}</ETag><ChecksumCRC32C>{c2}</ChecksumCRC32C></Part></CompleteMultipartUpload>",
        r1.header("etag").unwrap(),
        r2.header("etag").unwrap()
    );
    let r = complete(&c, "comp", &id, body.into_bytes(), &[]).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let mut cat = Algorithm::Crc32c.hash(&p1);
    cat.extend(Algorithm::Crc32c.hash(&p2));
    let expected = format!("{}-2", b64(&Algorithm::Crc32c.hash(&cat)));
    assert_eq!(r.one("ChecksumCRC32C"), expected);
    assert_eq!(r.one("ChecksumType"), "COMPOSITE");
    let h = c
        .send(
            "HEAD",
            "/docs/comp",
            "",
            &[("x-amz-checksum-mode", "ENABLED")],
            Payload::Signed(vec![]),
        )
        .await;
    assert_eq!(h.header("x-amz-checksum-crc32c").unwrap(), expected);

    // Explicit FULL_OBJECT CRC32 with a full-object checksum on completion.
    let id = initiate(
        &c,
        "full",
        &[
            ("x-amz-checksum-algorithm", "CRC32"),
            ("x-amz-checksum-type", "FULL_OBJECT"),
        ],
    )
    .await;
    let e1 = part(&c, "full", &id, 1, &p1, &[])
        .await
        .header("etag")
        .unwrap();
    let e2 = part(&c, "full", &id, 2, &p2, &[])
        .await
        .header("etag")
        .unwrap();
    let wrong = complete(
        &c,
        "full",
        &id,
        manifest(&[(1, e1.clone()), (2, e2.clone())]),
        &[("x-amz-checksum-crc32", "AAAAAA==")],
    )
    .await;
    assert_eq!(wrong.code(), "BadDigest");
    let good = b64(&Algorithm::Crc32.hash(&whole));
    let r = complete(
        &c,
        "full",
        &id,
        manifest(&[(1, e1), (2, e2)]),
        &[("x-amz-checksum-crc32", &good)],
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.one("ChecksumCRC32"), good);
    // Unsupported combinations.
    let r = c
        .send(
            "POST",
            "/docs/x",
            "uploads",
            &[
                ("x-amz-checksum-algorithm", "SHA256"),
                ("x-amz-checksum-type", "FULL_OBJECT"),
            ],
            Payload::Signed(vec![]),
        )
        .await;
    assert_eq!(r.code(), "InvalidRequest");
    let r = c
        .send(
            "POST",
            "/docs/x",
            "uploads",
            &[
                ("x-amz-checksum-algorithm", "CRC64NVME"),
                ("x-amz-checksum-type", "COMPOSITE"),
            ],
            Payload::Signed(vec![]),
        )
        .await;
    assert_eq!(r.code(), "InvalidRequest");
}

#[tokio::test(flavor = "multi_thread")]
async fn mpu_06_completion_conditions_and_quota_at_commit() {
    let (s, c) = setup().await;
    c.put("/docs/exists", b"old").await;
    let id = initiate(&c, "exists", &[]).await;
    let e = part(&c, "exists", &id, 1, b"new", &[])
        .await
        .header("etag")
        .unwrap();
    let r = complete(
        &c,
        "exists",
        &id,
        manifest(&[(1, e.clone())]),
        &[("if-none-match", "*")],
    )
    .await;
    assert_eq!(r.code(), "PreconditionFailed");
    assert_eq!(c.get("/docs/exists", "").await.body, b"old");
    // The upload went back to OPEN and can still complete.
    let etag = c.head("/docs/exists").await.header("etag").unwrap();
    let r = complete(
        &c,
        "exists",
        &id,
        manifest(&[(1, e)]),
        &[("if-match", &etag)],
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(c.get("/docs/exists", "").await.body, b"new");
    // Quota at commit.
    s.store()
        .db
        .write(|conn| {
            litebucket::metadata::with_write_tx(conn, |tx| {
                litebucket::metadata::queries::set_bucket_quota(tx, "docs", Some(10))
            })
        })
        .await
        .unwrap();
    let id = initiate(&c, "q", &[]).await;
    let e = part(&c, "q", &id, 1, &data(100, 1), &[])
        .await
        .header("etag")
        .unwrap();
    assert_eq!(
        complete(&c, "q", &id, manifest(&[(1, e)]), &[])
            .await
            .code(),
        "QuotaExceeded"
    );
    assert_eq!(
        c.put("/docs/q2", &data(100, 1)).await.code(),
        "QuotaExceeded"
    );
    // Reads and deletes still work over quota.
    assert_eq!(c.get("/docs/exists", "").await.status, 200);
    assert_eq!(c.delete("/docs/exists").await.status, 204);
}

#[tokio::test(flavor = "multi_thread")]
async fn mpu_07_completion_retry_uses_receipt_without_resurrection() {
    let (_s, c) = setup().await;
    let id = initiate(&c, "r", &[]).await;
    let e = part(&c, "r", &id, 1, b"payload", &[])
        .await
        .header("etag")
        .unwrap();
    let body = manifest(&[(1, e.clone())]);
    let first = complete(&c, "r", &id, body.clone(), &[]).await;
    assert_eq!(first.status, 200);
    let retry = complete(&c, "r", &id, body.clone(), &[]).await;
    assert_eq!(retry.status, 200);
    assert_eq!(retry.one("ETag"), first.one("ETag"));
    // A changed manifest does not reuse the receipt.
    let other = manifest(&[(1, "\"ffffffffffffffffffffffffffffffff\"".into())]);
    assert_eq!(
        complete(&c, "r", &id, other, &[]).await.code(),
        "NoSuchUpload"
    );
    // Delete the object, then retry: receipt replays, object stays deleted.
    c.delete("/docs/r").await;
    assert_eq!(complete(&c, "r", &id, body.clone(), &[]).await.status, 200);
    assert_eq!(c.head("/docs/r").await.status, 404, "no resurrection");
    // Overwrite, then retry: the newer object remains.
    c.put("/docs/r", b"newer").await;
    complete(&c, "r", &id, body, &[]).await;
    assert_eq!(c.get("/docs/r", "").await.body, b"newer");
}

#[tokio::test(flavor = "multi_thread")]
async fn mpu_08_expiry_and_abort_release_parts() {
    let s = TestServer::start_with("[multipart]\ninactive_expiration_seconds = 60\nreceipt_retention_seconds = 0\n[maintenance]\ngarbage_grace_seconds = 0\n").await;
    let c = s.admin();
    c.create_bucket("docs").await;
    let store = s.store();
    let id = initiate(&c, "old", &[]).await;
    part(&c, "old", &id, 1, &data(1000, 1), &[]).await;
    let active = initiate(&c, "active", &[]).await;
    part(&c, "active", &active, 1, &data(1000, 1), &[]).await;
    assert_eq!(store.capacity.part_bytes(), 2000);
    // Age the first upload past the inactivity window.
    let idc = id.clone();
    store
        .db
        .write(move |conn| {
            conn.execute(
                "UPDATE multipart_uploads SET last_activity_ms = 0 WHERE upload_id = ?1",
                [idc],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    // An upload with in-flight work is skipped.
    {
        let _busy = store.upload_activity(&id);
        assert_eq!(
            litebucket::maintenance::expire_once(&store).await.unwrap(),
            0
        );
    }
    assert_eq!(
        litebucket::maintenance::expire_once(&store).await.unwrap(),
        1
    );
    assert_eq!(
        part(&c, "old", &id, 2, b"x", &[]).await.code(),
        "NoSuchUpload"
    );
    assert_eq!(store.capacity.part_bytes(), 1000);
    // Parts of expired/aborted uploads are reclaimed by GC; active ones stay.
    while litebucket::maintenance::gc_once(&store).await.unwrap() > 0 {}
    assert_eq!(files_under(&s.data_dir().join("multipart")).len(), 1);
    c.send(
        "DELETE",
        "/docs/active",
        &format!("uploadId={active}"),
        &[],
        Payload::Signed(vec![]),
    )
    .await;
    while litebucket::maintenance::gc_once(&store).await.unwrap() > 0 {}
    assert!(files_under(&s.data_dir().join("multipart")).is_empty());
    assert_eq!(store.capacity.part_bytes(), 0);
    // Expired receipts are removed.
    litebucket::maintenance::expire_once(&store).await.unwrap();
    let n: i64 = store
        .db
        .read(
            |conn| Ok(conn.query_row("SELECT count(*) FROM multipart_uploads", [], |r| r.get(0))?),
        )
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn cap_03_assembly_accounts_for_output_space() {
    let s = TestServer::start_with(
        "[limits]\nmax_temporary_bytes = 12582912\nmax_part_bytes = 5242880\nmax_single_put_bytes = 5242880\nmax_object_bytes = 52428800\n",
    )
    .await;
    let c = s.admin();
    c.create_bucket("docs").await;
    c.put("/docs/a", b"old").await;
    let id = initiate(&c, "a", &[]).await;
    let e1 = part(&c, "a", &id, 1, &data(5 * MIB, 1), &[])
        .await
        .header("etag")
        .unwrap();
    let e2 = part(&c, "a", &id, 2, &data(2 * MIB, 2), &[])
        .await
        .header("etag")
        .unwrap();
    // 7 MiB of parts + 7 MiB output exceeds the 12 MiB temporary cap.
    let r = complete(
        &c,
        "a",
        &id,
        manifest(&[(1, e1.clone()), (2, e2.clone())]),
        &[],
    )
    .await;
    assert_eq!(r.status, 503, "{}", r.text());
    assert_eq!(c.get("/docs/a", "").await.body, b"old");
    // Still OPEN after the refusal; listing works.
    assert_eq!(
        c.get("/docs/a", &format!("uploadId={id}"))
            .await
            .all("PartNumber")
            .len(),
        2
    );
}
