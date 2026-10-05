//! Authentication, authorization, listing, protocol and HTTP behavior
//! (AUTH-*, LIST-*, HTTP-*, DEP-02).

mod common;

use common::*;

async fn setup() -> (TestServer, Client) {
    let s = TestServer::start().await;
    let c = s.admin();
    assert_eq!(c.create_bucket("docs").await.status, 200);
    (s, c)
}

#[tokio::test(flavor = "multi_thread")]
async fn dep_02_anonymous_and_malformed_auth_fail_closed() {
    let (s, c) = setup().await;
    c.put("/docs/k", b"x").await;
    for (headers, want) in [
        (vec![], "AccessDenied"),
        (
            vec![("authorization", "AWS admin-key:abc")],
            "InvalidRequest",
        ),
        (
            vec![("authorization", "AWS4-ECDSA-P256-SHA256 Credential=x")],
            "InvalidRequest",
        ),
        (
            vec![("authorization", "AWS4-HMAC-SHA256 garbage")],
            "AuthorizationHeaderMalformed",
        ),
    ] {
        let r = raw("GET", &format!("{}/docs/k", s.base), &headers, vec![]).await;
        assert!(r.status == 403 || r.status == 400, "{headers:?}");
        assert_eq!(r.code(), want, "{headers:?}");
    }
    let r = raw(
        "GET",
        &format!("{}/docs/k?AWSAccessKeyId=x&Signature=y&Expires=1", s.base),
        &[],
        vec![],
    )
    .await;
    assert_eq!(r.code(), "InvalidRequest", "SigV2 query auth disabled");
    let r = raw("GET", &format!("{}/", s.base), &[], vec![]).await;
    assert_eq!(r.code(), "AccessDenied");
}

#[tokio::test(flavor = "multi_thread")]
async fn auth_01_signature_checks() {
    let (s, c) = setup().await;
    c.put("/docs/k", b"x").await;
    // Clock skew beyond 15 minutes.
    let (d, a) = amz_at(unix_now() - 3600);
    let r = c
        .send_at("GET", "/docs/k", "", &[], Payload::Signed(vec![]), &d, &a)
        .await;
    assert_eq!(r.code(), "RequestTimeTooSkewed");
    // Wrong region.
    let mut wrong = c.clone();
    wrong.region = "eu-west-1".into();
    let r = wrong.get("/docs/k", "").await;
    assert_eq!(r.code(), "AuthorizationHeaderMalformed");
    assert!(r.text().contains("<Region>us-east-1</Region>"));
    // Unknown key.
    let r = Client::new(&s.base, ("nobody", "secretsecretsecretsecretsecretsec1"))
        .get("/docs/k", "")
        .await;
    assert_eq!(r.code(), "InvalidAccessKeyId");
    // Unsigned x-amz-* header is rejected.
    let url = format!("{}/docs/k", s.base);
    let signed = c.presign("GET", "/docs/k", 60, None);
    let r = raw(
        "GET",
        &signed,
        &[("x-amz-checksum-mode", "ENABLED")],
        vec![],
    )
    .await;
    assert_eq!(r.code(), "AccessDenied");
    let _ = url;
    // Temporary-credential tokens are not accepted.
    let r = c
        .send(
            "GET",
            "/docs/k",
            "",
            &[("x-amz-security-token", "t")],
            Payload::Signed(vec![]),
        )
        .await;
    assert_eq!(r.code(), "InvalidRequest");
}

#[tokio::test(flavor = "multi_thread")]
async fn auth_01_presigned_get_put_head() {
    let (s, c) = setup().await;
    let put = c.presign("PUT", "/docs/pre", 300, None);
    let r = raw("PUT", &put, &[], b"presigned body".to_vec()).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let get = c.presign("GET", "/docs/pre", 300, None);
    assert_eq!(raw("GET", &get, &[], vec![]).await.body, b"presigned body");
    // A GET signature cannot be used for HEAD or PUT.
    assert_eq!(raw("HEAD", &get, &[], vec![]).await.status, 403);
    assert_eq!(raw("PUT", &get, &[], b"x".to_vec()).await.status, 403);
    let head = c.presign("HEAD", "/docs/pre", 300, None);
    let h = raw("HEAD", &head, &[], vec![]).await;
    assert_eq!(h.status, 200);
    assert_eq!(h.header("content-length").unwrap(), "14");
    // Expired URL.
    let old = c.presign("GET", "/docs/pre", 60, Some(unix_now() - 3600));
    let r = raw("GET", &old, &[], vec![]).await;
    assert_eq!(r.status, 403);
    assert!(r.text().contains("expired"));
    // A long-lived URL signed an hour ago is still valid (no skew rule).
    let long = c.presign("GET", "/docs/pre", 7200, Some(unix_now() - 3600));
    assert_eq!(raw("GET", &long, &[], vec![]).await.status, 200);
    // Future-dated beyond the skew window.
    let future = c.presign("GET", "/docs/pre", 60, Some(unix_now() + 3600));
    assert_eq!(raw("GET", &future, &[], vec![]).await.status, 403);
    // Expiry beyond seven days is invalid.
    let too_long = c.presign("GET", "/docs/pre", 700_000, None);
    assert_eq!(
        raw("GET", &too_long, &[], vec![]).await.code(),
        "AuthorizationQueryParametersError"
    );
    // Tampered signature.
    let tampered = get.replace("X-Amz-Signature=", "X-Amz-Signature=0");
    assert_eq!(raw("GET", &tampered, &[], vec![]).await.status, 403);
    let _ = s;
}

#[tokio::test(flavor = "multi_thread")]
async fn auth_02_04_prefix_scoped_grants_fail_closed() {
    let (s, c) = setup().await;
    c.create_bucket("private").await;
    for k in ["customers/123/a", "customers/1234/b", "other/c"] {
        c.put(&format!("/docs/{k}"), b"x").await;
    }
    let reader = s.client(READER);
    assert_eq!(reader.get("/docs/customers/123/a", "").await.status, 200);
    assert_eq!(
        reader.get("/docs/customers/1234/b", "").await.code(),
        "AccessDenied"
    );
    assert_eq!(reader.get("/docs/other/c", "").await.code(), "AccessDenied");
    assert_eq!(
        reader.put("/docs/customers/123/new", b"x").await.code(),
        "AccessDenied"
    );
    assert_eq!(
        reader.delete("/docs/customers/123/a").await.code(),
        "AccessDenied"
    );
    // Missing key inside a listable prefix is NoSuchKey; outside it, AccessDenied.
    assert_eq!(
        reader.get("/docs/customers/123/missing", "").await.code(),
        "NoSuchKey"
    );
    assert_eq!(
        reader.get("/docs/other/missing", "").await.code(),
        "AccessDenied"
    );
    // Listing must be contained in a granted prefix.
    let ok = reader
        .get("/docs", "list-type=2&prefix=customers%2F123%2F")
        .await;
    assert_eq!(ok.status, 200);
    assert_eq!(ok.all("Key"), vec!["customers/123/a"]);
    for p in ["", "customers%2F", "customers%2F1234%2F", "customers%2F12"] {
        let r = reader
            .get("/docs", &format!("list-type=2&prefix={p}"))
            .await;
        assert_eq!(r.code(), "AccessDenied", "prefix {p}");
    }
    // No bucket administration, no bucket creation, no enumeration.
    assert_eq!(
        reader.create_bucket("newbucket").await.code(),
        "AccessDenied"
    );
    assert_eq!(reader.delete("/docs").await.code(), "AccessDenied");
    assert_eq!(reader.get("/", "").await.code(), "AccessDenied");
    assert_eq!(
        reader.get("/private", "list-type=2").await.code(),
        "AccessDenied"
    );
    // HeadBucket discloses only buckets with a grant.
    assert_eq!(reader.head("/docs").await.status, 200);
    assert_eq!(reader.head("/private").await.status, 403);
    // App credential: whole-bucket grant plus list_buckets sees only its buckets.
    let app = s.client(APP);
    let r = app.get("/", "").await;
    assert_eq!(r.all("Name"), vec!["docs"]);
    assert_eq!(app.create_bucket("x-new").await.code(), "AccessDenied");
    assert_eq!(
        app.delete("/docs").await.code(),
        "AccessDenied",
        "no manage_bucket"
    );
    // Listing tokens and upload IDs are not permissions.
    let page = c.get("/docs", "list-type=2&max-keys=1").await;
    let token = page.one("NextContinuationToken");
    let r = reader
        .get(
            "/docs",
            &format!("list-type=2&continuation-token={}", encode_q(&token)),
        )
        .await;
    assert_eq!(r.code(), "AccessDenied");
    let up = c
        .send(
            "POST",
            "/docs/other/mp",
            "uploads",
            &[],
            Payload::Signed(vec![]),
        )
        .await;
    let id = up.one("UploadId");
    let r = reader
        .send(
            "PUT",
            "/docs/other/mp",
            &format!("partNumber=1&uploadId={id}"),
            &[],
            Payload::Signed(b"x".to_vec()),
        )
        .await;
    assert_eq!(r.code(), "AccessDenied");
    let r = reader
        .send(
            "DELETE",
            "/docs/other/mp",
            &format!("uploadId={id}"),
            &[],
            Payload::Signed(vec![]),
        )
        .await;
    assert_eq!(r.code(), "AccessDenied");
}

#[tokio::test(flavor = "multi_thread")]
async fn auth_05_unsupported_protection_features_fail() {
    let (_s, c) = setup().await;
    for (k, v) in [
        ("x-amz-server-side-encryption", "AES256"),
        ("x-amz-server-side-encryption-customer-algorithm", "AES256"),
        ("x-amz-object-lock-mode", "COMPLIANCE"),
        ("x-amz-object-lock-legal-hold", "ON"),
        ("x-amz-tagging", "a=b"),
        ("x-amz-acl", "public-read"),
        (
            "x-amz-grant-read",
            "uri=http://acs.amazonaws.com/groups/global/AllUsers",
        ),
        ("x-amz-storage-class", "GLACIER"),
    ] {
        let r = c.put_h("/docs/p", &[(k, v)], b"x").await;
        assert!(r.status >= 400, "{k}");
        assert_eq!(c.head("/docs/p").await.status, 404, "{k} must not publish");
    }
    for q in [
        "acl",
        "versioning",
        "tagging",
        "retention",
        "legal-hold",
        "policy",
        "lifecycle",
        "encryption",
        "object-lock",
    ] {
        let path = if matches!(q, "acl" | "tagging" | "retention" | "legal-hold") {
            "/docs/p"
        } else {
            "/docs"
        };
        let r = c
            .send("PUT", path, q, &[], Payload::Signed(b"<x/>".to_vec()))
            .await;
        assert_eq!(r.code(), "NotImplemented", "{q}");
    }
    assert_eq!(
        c.get("/docs/p", "versionId=abc").await.code(),
        "NotImplemented"
    );
    assert_eq!(
        c.get("/docs", "").await.code(),
        "NotImplemented",
        "ListObjects v1 is deferred"
    );
    // Accepted no-op canned ACLs.
    assert_eq!(
        c.put_h("/docs/ok", &[("x-amz-acl", "private")], b"x")
            .await
            .status,
        200
    );
    assert_eq!(
        c.put_h(
            "/docs/ok2",
            &[("x-amz-acl", "bucket-owner-full-control")],
            b"x"
        )
        .await
        .status,
        200
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn list_01_02_03_listing_contract() {
    let (s, c) = setup().await;
    let keys = [
        "a.txt",
        "dir/1",
        "dir/2",
        "dir/sub/3",
        "dir2/x",
        "e%_",
        "e%a",
        "z",
        "é",
        "dir/sub/4",
    ];
    for k in keys {
        c.put(&format!("/docs/{}", encode_key(k)), b"x").await;
    }
    let r = c.get("/docs", "list-type=2&delimiter=%2F").await;
    assert_eq!(r.all("Key"), vec!["a.txt", "e%_", "e%a", "z", "é"]);
    assert_eq!(r.all("Prefix")[1..].to_vec(), vec!["dir/", "dir2/"]);
    assert_eq!(r.one("KeyCount"), "7");
    let r = c
        .get("/docs", "list-type=2&prefix=dir%2F&delimiter=%2F")
        .await;
    assert_eq!(r.all("Key"), vec!["dir/1", "dir/2"]);
    assert!(r.all("Prefix").contains(&"dir/sub/".to_string()));
    // LIKE wildcards are literal.
    let r = c.get("/docs", "list-type=2&prefix=e%25_").await;
    assert_eq!(r.all("Key"), vec!["e%_"]);
    // start-after.
    let r = c
        .get("/docs", "list-type=2&start-after=dir%2Fsub%2F3")
        .await;
    assert_eq!(r.all("Key")[0], "dir/sub/4");
    // Paging with max-keys=2 visits every entry exactly once, groups included.
    let mut all = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let q = match &token {
            Some(t) => format!(
                "list-type=2&delimiter=%2F&max-keys=2&continuation-token={}",
                encode_q(t)
            ),
            None => "list-type=2&delimiter=%2F&max-keys=2".into(),
        };
        let r = c.get("/docs", &q).await;
        assert_eq!(r.status, 200, "{}", r.text());
        all.extend(r.all("Key"));
        all.extend(r.all("Prefix").into_iter().filter(|p| p.ends_with('/')));
        if r.one("IsTruncated") != "true" {
            break;
        }
        token = Some(r.one("NextContinuationToken"));
    }
    all.sort();
    let mut want = vec!["a.txt", "dir/", "dir2/", "e%_", "e%a", "z", "é"];
    want.sort();
    assert_eq!(all, want);
    // max-keys=0 is an empty, non-truncated page.
    let r = c.get("/docs", "list-type=2&max-keys=0").await;
    assert_eq!(
        (r.one("KeyCount").as_str(), r.one("IsTruncated").as_str()),
        ("0", "false")
    );
    // Tokens are bound to bucket, options, and principal.
    let page = c.get("/docs", "list-type=2&max-keys=1").await;
    let t = page.one("NextContinuationToken");
    let r = c
        .get(
            "/docs",
            &format!(
                "list-type=2&max-keys=1&delimiter=%2F&continuation-token={}",
                encode_q(&t)
            ),
        )
        .await;
    assert_eq!(r.code(), "InvalidArgument", "changed delimiter");
    let mut tampered = t.clone();
    tampered.replace_range(2..3, if &t[2..3] == "A" { "B" } else { "A" });
    let r = c
        .get(
            "/docs",
            &format!("list-type=2&continuation-token={}", encode_q(&tampered)),
        )
        .await;
    assert_eq!(r.code(), "InvalidArgument");
    c.create_bucket("docs2").await;
    let r = c
        .get(
            "/docs2",
            &format!("list-type=2&max-keys=1&continuation-token={}", encode_q(&t)),
        )
        .await;
    assert_eq!(r.code(), "InvalidArgument", "token from another bucket");
    let app = s.client(APP);
    let r = app
        .get(
            "/docs",
            &format!("list-type=2&max-keys=1&continuation-token={}", encode_q(&t)),
        )
        .await;
    assert_eq!(r.code(), "InvalidArgument", "token from another principal");
    // fetch-owner and encoding-type=url.
    let r = c
        .get(
            "/docs",
            "list-type=2&fetch-owner=true&encoding-type=url&prefix=%C3%A9",
        )
        .await;
    assert_eq!(r.all("Key"), vec!["%C3%A9"]);
    assert!(r.text().contains("<Owner><ID>"));
}

#[tokio::test(flavor = "multi_thread")]
async fn list_02_recreated_bucket_rejects_old_tokens() {
    let (_s, c) = setup().await;
    c.put("/docs/a", b"x").await;
    c.put("/docs/b", b"x").await;
    let t = c
        .get("/docs", "list-type=2&max-keys=1")
        .await
        .one("NextContinuationToken");
    c.delete("/docs/a").await;
    c.delete("/docs/b").await;
    assert_eq!(c.delete("/docs").await.status, 204);
    c.create_bucket("docs").await;
    c.put("/docs/a", b"x").await;
    let r = c
        .get(
            "/docs",
            &format!("list-type=2&continuation-token={}", encode_q(&t)),
        )
        .await;
    assert_eq!(r.code(), "InvalidArgument");
}

#[tokio::test(flavor = "multi_thread")]
async fn bucket_operations_and_errors() {
    let (s, c) = setup().await;
    assert_eq!(
        c.create_bucket("docs").await.code(),
        "BucketAlreadyOwnedByYou"
    );
    assert_eq!(
        c.create_bucket("Bad_Name").await.code(),
        "InvalidBucketName"
    );
    let r = c.get("/docs", "location").await;
    assert!(r.text().contains("<LocationConstraint xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"></LocationConstraint>"));
    let body = b"<CreateBucketConfiguration><LocationConstraint>eu-west-1</LocationConstraint></CreateBucketConfiguration>";
    let r = c
        .send("PUT", "/eubucket", "", &[], Payload::Signed(body.to_vec()))
        .await;
    assert_eq!(r.code(), "IllegalLocationConstraintException");
    let h = c.head("/docs").await;
    assert_eq!(h.header("x-amz-bucket-region").unwrap(), "us-east-1");
    assert_eq!(c.head("/nope").await.status, 404);
    c.put("/docs/k", b"x").await;
    assert_eq!(c.delete("/docs").await.code(), "BucketNotEmpty");
    let up = c
        .send("POST", "/docs/m", "uploads", &[], Payload::Signed(vec![]))
        .await;
    c.delete("/docs/k").await;
    assert_eq!(
        c.delete("/docs").await.code(),
        "BucketNotEmpty",
        "active multipart upload"
    );
    c.send(
        "DELETE",
        "/docs/m",
        &format!("uploadId={}", up.one("UploadId")),
        &[],
        Payload::Signed(vec![]),
    )
    .await;
    assert_eq!(c.delete("/docs").await.status, 204);
    assert_eq!(c.get("/docs", "list-type=2").await.code(), "NoSuchBucket");
    // Error responses are S3 XML with a request ID and no internals.
    let r = c.get("/nope/key", "").await;
    assert_eq!(r.header("content-type").unwrap(), "application/xml");
    assert!(r.text().contains("<RequestId>"));
    assert!(r.header("x-amz-request-id").is_some());
    assert!(
        !r.text().contains("sqlite") && !r.text().contains(&s.data_dir().display().to_string())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn http_01_unknown_parameters_and_methods() {
    let (_s, c) = setup().await;
    let r = c.get("/docs", "list-type=2&bogus=1").await;
    assert_eq!(r.code(), "InvalidArgument");
    let r = c
        .send("PATCH", "/docs/k", "", &[], Payload::Signed(vec![]))
        .await;
    assert_eq!(r.code(), "MethodNotAllowed");
    let r = c
        .send(
            "PUT",
            "/docs/k",
            "",
            &[("x-amz-new-feature", "1")],
            Payload::Signed(b"x".to_vec()),
        )
        .await;
    assert_eq!(r.code(), "NotImplemented");
    // x-id is a benign SDK operation hint.
    assert_eq!(
        c.send(
            "PUT",
            "/docs/k",
            "x-id=PutObject",
            &[],
            Payload::Signed(b"x".to_vec())
        )
        .await
        .status,
        200
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn http_03_request_bounds() {
    let (s, c) = setup().await;
    let long_key = "k".repeat(1025);
    assert_eq!(c.put(&format!("/docs/{long_key}"), b"x").await.status, 400);
    let huge = "a".repeat(20_000);
    let r = raw("GET", &format!("{}/docs/{huge}", s.base), &[], vec![]).await;
    assert!(
        r.status == 414 || r.status == 400 || r.status == 431,
        "{}",
        r.status
    );
    let mut headers = Vec::new();
    let names: Vec<String> = (0..200).map(|i| format!("x-h{i}")).collect();
    for n in &names {
        headers.push((n.as_str(), "v"));
    }
    let r = raw("GET", &format!("{}/docs/k", s.base), &headers, vec![]).await;
    assert!(r.status == 431 || r.status == 400, "{}", r.status);
    // Oversized XML control body.
    let body = format!(
        "<Delete>{}</Delete>",
        "<Object><Key>k</Key></Object>".repeat(200_000)
    );
    let r = c
        .send(
            "POST",
            "/docs",
            "delete",
            &[(
                "content-md5",
                &litebucket::checksums::md5_b64(body.as_bytes()),
            )],
            Payload::Signed(body.into_bytes()),
        )
        .await;
    assert!(r.status == 400 || r.status == 0, "{}", r.text());
    assert_eq!(
        c.get("/docs", "list-type=2").await.status,
        200,
        "server still healthy"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn http_02_cors_preflight_and_actual_requests() {
    let (s, c) = setup().await;
    let cfg = br#"<CORSConfiguration><CORSRule><AllowedOrigin>https://app.example.com</AllowedOrigin><AllowedMethod>PUT</AllowedMethod><AllowedMethod>GET</AllowedMethod><AllowedHeader>*</AllowedHeader><ExposeHeader>ETag</ExposeHeader><MaxAgeSeconds>600</MaxAgeSeconds></CORSRule></CORSConfiguration>"#;
    let preflight = |origin: &'static str, method: &'static str| {
        let url = format!("{}/docs/upload.bin", s.base);
        async move {
            raw(
                "OPTIONS",
                &url,
                &[
                    ("origin", origin),
                    ("access-control-request-method", method),
                    ("access-control-request-headers", "content-type,x-amz-date"),
                ],
                vec![],
            )
            .await
        }
    };
    // No configuration: denied.
    assert_eq!(
        preflight("https://app.example.com", "PUT").await.status,
        403
    );
    let r = c
        .send(
            "PUT",
            "/docs",
            "cors",
            &[("content-md5", &litebucket::checksums::md5_b64(cfg))],
            Payload::Signed(cfg.to_vec()),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let r = c.get("/docs", "cors").await;
    assert!(
        r.text()
            .contains("<AllowedOrigin>https://app.example.com</AllowedOrigin>")
    );
    let ok = preflight("https://app.example.com", "PUT").await;
    assert_eq!(ok.status, 200);
    assert_eq!(
        ok.header("access-control-allow-origin").unwrap(),
        "https://app.example.com"
    );
    assert_eq!(ok.header("access-control-max-age").unwrap(), "600");
    assert!(ok.header("vary").unwrap().contains("Origin"));
    let bad = preflight("https://evil.example", "PUT").await;
    assert_eq!(bad.status, 403);
    assert!(bad.header("access-control-allow-origin").is_none());
    assert_eq!(
        preflight("https://app.example.com", "DELETE").await.status,
        403
    );
    // Preflight never creates the object; CORS does not bypass authentication.
    assert_eq!(c.head("/docs/upload.bin").await.status, 404);
    let r = raw(
        "PUT",
        &format!("{}/docs/upload.bin", s.base),
        &[("origin", "https://app.example.com")],
        b"x".to_vec(),
    )
    .await;
    assert_eq!(r.status, 403);
    // Actual signed request from an allowed origin gets CORS headers.
    let r = c
        .send(
            "PUT",
            "/docs/upload.bin",
            "",
            &[("origin", "https://app.example.com")],
            Payload::Signed(b"x".to_vec()),
        )
        .await;
    assert_eq!(r.status, 200);
    assert_eq!(
        r.header("access-control-allow-origin").unwrap(),
        "https://app.example.com"
    );
    assert_eq!(r.header("access-control-expose-headers").unwrap(), "ETag");
    let r = c
        .send(
            "GET",
            "/docs/upload.bin",
            "",
            &[("origin", "https://evil.example")],
            Payload::Signed(vec![]),
        )
        .await;
    assert!(r.header("access-control-allow-origin").is_none());
    assert_eq!(
        c.send("DELETE", "/docs", "cors", &[], Payload::Signed(vec![]))
            .await
            .status,
        204
    );
    assert_eq!(
        c.get("/docs", "cors").await.code(),
        "NoSuchCORSConfiguration"
    );
    // Prefix-scoped credentials cannot manage CORS.
    let reader = s.client(READER);
    assert_eq!(reader.get("/docs", "cors").await.code(), "AccessDenied");
}

#[tokio::test(flavor = "multi_thread")]
async fn auth_03_key_changes_apply_to_the_next_request() {
    let (s, c) = setup().await;
    assert_eq!(c.put("/docs/k", b"x").await.status, 200);
    let url = c.presign("GET", "/docs/k", 600, None);
    // A rejected change keeps the previous key set.
    let err = s
        .admin_api(
            "PATCH",
            "/v1/keys/admin-key",
            serde_json::json!({"expires_at": "soon"}),
        )
        .await
        .unwrap_err();
    assert!(err.contains("invalid expires_at"), "{err}");
    assert_eq!(c.get("/docs/k", "").await.status, 200);
    // Disabling revokes the key, including its presigned URLs, immediately.
    s.admin_api(
        "PATCH",
        "/v1/keys/admin-key",
        serde_json::json!({"enabled": false}),
    )
    .await
    .unwrap();
    assert_eq!(c.get("/docs/k", "").await.code(), "InvalidAccessKeyId");
    assert_eq!(raw("GET", &url, &[], vec![]).await.status, 403);
    assert_eq!(s.client(APP).get("/docs/k", "").await.status, 200);
    s.admin_api(
        "PATCH",
        "/v1/keys/admin-key",
        serde_json::json!({"enabled": true}),
    )
    .await
    .unwrap();
    assert_eq!(c.get("/docs/k", "").await.status, 200);
}

#[tokio::test(flavor = "multi_thread")]
async fn management_endpoints() {
    let (s, c) = setup().await;
    c.put("/docs/k", b"x").await;
    let r = raw("GET", &format!("{}/livez", s.mgmt), &[], vec![]).await;
    assert_eq!(r.status, 200);
    let r = raw("GET", &format!("{}/readyz", s.mgmt), &[], vec![]).await;
    assert_eq!(r.status, 200);
    assert!(r.text().contains("\"state\":\"writable\""));
    let m = raw("GET", &format!("{}/metrics", s.mgmt), &[], vec![]).await;
    let text = m.text();
    assert!(text.contains("litebucket_requests_total{operation=\"PutObject\",status=\"2xx\"}"));
    assert!(
        !text.contains("docs/k") && !text.contains("admin-key"),
        "no keys or credential labels"
    );
    // The S3 listener does not serve management paths as such.
    assert_ne!(
        raw("GET", &format!("{}/metrics", s.base), &[], vec![])
            .await
            .status,
        200
    );
}
