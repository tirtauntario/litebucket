//! Access keys in SQLite and the admin API: key lifecycle, grants, rotation,
//! buckets, audit, secret protection modes, offline recovery, and the CLI
//! client over the real Unix socket.

mod common;

use common::*;
use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_litebucket");

async fn setup() -> (TestServer, Client) {
    let s = TestServer::start().await;
    let c = s.admin();
    assert_eq!(c.create_bucket("docs").await.status, 200);
    (s, c)
}

fn issued(v: &Value) -> (String, String) {
    (
        v["access_key_id"].as_str().unwrap().to_string(),
        v["secret_access_key"].as_str().unwrap().to_string(),
    )
}

/// Owned client for a key issued at runtime (the harness takes &'static str).
fn client_for(s: &TestServer, id: &str, secret: &str) -> Client {
    let id: &'static str = Box::leak(id.to_string().into_boxed_str());
    let secret: &'static str = Box::leak(secret.to_string().into_boxed_str());
    s.client((id, secret))
}

#[tokio::test(flavor = "multi_thread")]
async fn key_lifecycle_applies_to_the_next_request() {
    let (s, admin) = setup().await;
    admin.put("/docs/a", b"a").await;
    let v = s
        .admin_api(
            "POST",
            "/v1/keys",
            json!({"description": "app", "grants": [{"bucket": "docs", "actions": ["read", "list"]}]}),
        )
        .await
        .unwrap();
    let (id, secret) = issued(&v);
    assert!(id.starts_with("SL") && id.len() == 20, "{id}");
    assert_eq!(secret.len(), 43);
    let k = client_for(&s, &id, &secret);
    assert_eq!(k.get("/docs/a", "").await.body, b"a");
    assert_eq!(k.put("/docs/b", b"b").await.code(), "AccessDenied");

    // Listing and showing never reveal secrets.
    let list = s.admin_api("GET", "/v1/keys", json!({})).await.unwrap();
    assert!(!list.to_string().contains(&secret));
    let shown = s
        .admin_api("GET", &format!("/v1/keys/{id}"), json!({}))
        .await
        .unwrap();
    assert_eq!(shown["grants"][0]["actions"], json!(["read", "list"]));
    assert!(!shown.to_string().contains(&secret));

    // Grants: add write, then remove the bucket grant entirely.
    s.admin_api(
        "POST",
        &format!("/v1/keys/{id}/grants"),
        json!({"bucket": "docs", "actions": ["read", "list", "write"]}),
    )
    .await
    .unwrap();
    assert_eq!(k.put("/docs/b", b"b").await.status, 200);
    s.admin_api(
        "POST",
        &format!("/v1/keys/{id}/grants/remove"),
        json!({"bucket": "docs"}),
    )
    .await
    .unwrap();
    assert_eq!(k.get("/docs/a", "").await.code(), "AccessDenied");

    // Expiry in the past makes the key unusable.
    s.admin_api(
        "PATCH",
        &format!("/v1/keys/{id}"),
        json!({"expires_at": "2001-01-01T00:00:00Z"}),
    )
    .await
    .unwrap();
    assert_eq!(k.get("/docs/a", "").await.code(), "InvalidAccessKeyId");
    s.admin_api(
        "PATCH",
        &format!("/v1/keys/{id}"),
        json!({"clear_expiry": true}),
    )
    .await
    .unwrap();

    // Delete.
    s.admin_api("DELETE", &format!("/v1/keys/{id}"), json!({}))
        .await
        .unwrap();
    assert_eq!(k.get("/docs/a", "").await.code(), "InvalidAccessKeyId");
    let err = s
        .admin_api("GET", &format!("/v1/keys/{id}"), json!({}))
        .await
        .unwrap_err();
    assert!(err.contains("no access key"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn validation_and_conflicts() {
    let (s, _) = setup().await;
    for (body, needle) in [
        (json!({"access_key_id": "x"}), "access key ids"),
        (json!({"global_grants": ["root"]}), "unknown global grant"),
        (
            json!({"grants": [{"bucket": "docs", "prefix": "a/", "actions": ["manage_bucket"]}]}),
            "manage_bucket",
        ),
        (
            json!({"grants": [{"bucket": "Bad_Name", "actions": ["read"]}]}),
            "grant bucket",
        ),
        (json!({"expires_at": "tomorrow"}), "invalid expires_at"),
        (json!({"access_key_id": "admin-key"}), "already exists"),
    ] {
        let err = s.admin_api("POST", "/v1/keys", body).await.unwrap_err();
        assert!(err.contains(needle), "{err} should mention {needle}");
    }
    let err = s
        .admin_api("POST", "/v1/keys/nope/rotate", json!({}))
        .await
        .unwrap_err();
    assert!(err.contains("no access key"), "{err}");
    let err = s
        .admin_api("GET", "/v1/nothing", json!({}))
        .await
        .unwrap_err();
    assert!(err.contains("no such admin endpoint"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn last_admin_key_cannot_be_removed() {
    let (s, _) = setup().await;
    // `init` created one generated admin key; the harness added admin-key.
    let keys = s.admin_api("GET", "/v1/keys", json!({})).await.unwrap();
    let admins: Vec<String> = keys
        .as_array()
        .unwrap()
        .iter()
        .filter(|k| {
            k["global_grants"]
                .as_array()
                .unwrap()
                .contains(&json!("admin"))
        })
        .map(|k| k["access_key_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(admins.len(), 2);
    s.admin_api("DELETE", &format!("/v1/keys/{}", admins[0]), json!({}))
        .await
        .unwrap();
    let last = &admins[1];
    for (m, p, b) in [
        ("DELETE", format!("/v1/keys/{last}"), json!({})),
        (
            "PATCH",
            format!("/v1/keys/{last}"),
            json!({"enabled": false}),
        ),
        (
            "DELETE",
            format!("/v1/keys/{last}/global-grants/admin"),
            json!({}),
        ),
    ] {
        let err = s.admin_api(m, &p, b).await.unwrap_err();
        assert!(err.contains("no enabled admin key"), "{err}");
    }
    assert_eq!(s.admin().get("/", "").await.status, 200, "still usable");
}

#[tokio::test(flavor = "multi_thread")]
async fn rotation_with_and_without_grace() {
    let (s, admin) = setup().await;
    admin.put("/docs/a", b"a").await;
    let v = s
        .admin_api(
            "POST",
            "/v1/keys",
            json!({"grants": [{"bucket": "docs", "actions": ["read"]}]}),
        )
        .await
        .unwrap();
    let (id, old) = issued(&v);
    let v = s
        .admin_api(
            "POST",
            &format!("/v1/keys/{id}/rotate"),
            json!({"grace_seconds": 3600}),
        )
        .await
        .unwrap();
    let (_, new) = issued(&v);
    assert!(v["previous_secret_valid_until"].is_string());
    assert_ne!(old, new);
    assert_eq!(
        client_for(&s, &id, &old).get("/docs/a", "").await.status,
        200
    );
    assert_eq!(
        client_for(&s, &id, &new).get("/docs/a", "").await.status,
        200
    );
    // Survives a restart (both secrets stored encrypted).
    let mut s = s;
    s.restart().await;
    assert_eq!(
        client_for(&s, &id, &old).get("/docs/a", "").await.status,
        200
    );
    // Rotating again without grace revokes every older secret now.
    let v = s
        .admin_api("POST", &format!("/v1/keys/{id}/rotate"), json!({}))
        .await
        .unwrap();
    let (_, newest) = issued(&v);
    assert_eq!(
        client_for(&s, &id, &new).get("/docs/a", "").await.code(),
        "SignatureDoesNotMatch"
    );
    assert_eq!(
        client_for(&s, &id, &old).get("/docs/a", "").await.code(),
        "SignatureDoesNotMatch"
    );
    assert_eq!(
        client_for(&s, &id, &newest).get("/docs/a", "").await.status,
        200
    );
    let err = s
        .admin_api(
            "POST",
            &format!("/v1/keys/{id}/rotate"),
            json!({"grace_seconds": 31 * 86400}),
        )
        .await
        .unwrap_err();
    assert!(err.contains("grace period"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn buckets_quota_and_cors() {
    let (s, admin) = setup().await;
    let b = s
        .admin_api(
            "POST",
            "/v1/buckets",
            json!({"name": "media", "quota_bytes": 10,
                   "cors": [{"allowed_origins": ["https://app.example.com"], "allowed_methods": ["GET"]}]}),
        )
        .await
        .unwrap();
    assert_eq!(b["quota_bytes"], 10);
    assert_eq!(admin.put("/media/a", b"0123456789").await.status, 200);
    assert_eq!(admin.put("/media/b", b"x").await.code(), "QuotaExceeded");
    let cors = admin.get("/media", "cors").await;
    assert!(
        cors.text().contains("https://app.example.com"),
        "{}",
        cors.text()
    );
    s.admin_api(
        "PUT",
        "/v1/buckets/media/quota",
        json!({"quota_bytes": null}),
    )
    .await
    .unwrap();
    assert_eq!(admin.put("/media/b", b"x").await.status, 200);
    s.admin_api("DELETE", "/v1/buckets/media/cors", json!({}))
        .await
        .unwrap();
    assert_eq!(
        admin.get("/media", "cors").await.code(),
        "NoSuchCORSConfiguration"
    );
    let err = s
        .admin_api(
            "PUT",
            "/v1/buckets/media/cors",
            json!({"rules": [{"allowed_origins": ["*"], "allowed_methods": ["PATCH"]}]}),
        )
        .await
        .unwrap_err();
    assert!(err.contains("allowed method"), "{err}");
    let err = s
        .admin_api("DELETE", "/v1/buckets/media", json!({}))
        .await
        .unwrap_err();
    assert!(err.contains("not empty"), "{err}");
    let err = s
        .admin_api("POST", "/v1/buckets", json!({"name": "media"}))
        .await
        .unwrap_err();
    assert!(err.contains("already exists"), "{err}");
    admin.delete("/media/a").await;
    admin.delete("/media/b").await;
    s.admin_api("DELETE", "/v1/buckets/media", json!({}))
        .await
        .unwrap();
    let list = s.admin_api("GET", "/v1/buckets", json!({})).await.unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["name"], "docs");
}

#[tokio::test(flavor = "multi_thread")]
async fn audit_records_changes_without_secrets() {
    let (s, _) = setup().await;
    let v = s.admin_api("POST", "/v1/keys", json!({})).await.unwrap();
    let (id, secret) = issued(&v);
    s.admin_api("POST", &format!("/v1/keys/{id}/rotate"), json!({}))
        .await
        .unwrap();
    let audit = s
        .admin_api("GET", "/v1/audit?limit=10", json!({}))
        .await
        .unwrap();
    let text = audit.to_string();
    assert!(!text.contains(&secret));
    assert_eq!(audit[0]["action"], "key.rotate");
    assert_eq!(audit[1]["action"], "key.create");
    assert_eq!(audit[0]["target"], id.as_str());
    assert!(audit[0]["actor"].as_str().unwrap().starts_with("uid:"));
    let status = s.admin_api("GET", "/v1/status", json!({})).await.unwrap();
    assert_eq!(status["secret_protection"], "encrypted");
    assert_eq!(status["buckets"], 1);
}

fn db_contains(s: &TestServer, needle: &str) -> bool {
    let mut found = false;
    for f in ["metadata.sqlite3", "metadata.sqlite3-wal"] {
        if let Ok(bytes) = std::fs::read(s.data_dir().join(f)) {
            found |= bytes.windows(needle.len()).any(|w| w == needle.as_bytes());
        }
    }
    found
}

fn set_protection(s: &TestServer, mode: &str) {
    let text: String = std::fs::read_to_string(&s.config_path)
        .unwrap()
        .lines()
        .filter(|l| !l.starts_with("protection ="))
        .map(|l| format!("{l}\n"))
        .collect();
    let text = text.replacen(
        "[secrets]\n",
        &format!("[secrets]\nprotection = \"{mode}\"\n"),
        1,
    );
    std::fs::write(&s.config_path, text).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn protection_modes_convert_at_startup() {
    let (mut s, admin) = setup().await;
    admin.put("/docs/a", b"a").await;
    s.stop().await;
    assert!(!db_contains(&s, ADMIN.1), "encrypted at rest by default");

    set_protection(&s, "plaintext");
    s.boot().await;
    assert_eq!(s.admin().get("/docs/a", "").await.status, 200);
    s.stop().await;
    assert!(
        db_contains(&s, ADMIN.1),
        "plaintext mode stores the secret as-is"
    );
    let cfg = load_config(&s.config_path);
    assert!(litebucket::doctor::doctor(&cfg, false).unwrap());

    set_protection(&s, "encrypted");
    // Encrypt under a new key, then put the old key back: startup refuses
    // because the stored secrets no longer open with the configured key.
    let key = s.dir.path().join("master.key");
    let saved = std::fs::read(&key).unwrap();
    std::fs::remove_file(&key).unwrap();
    litebucket::secrets::MasterKey::generate(&key).unwrap();
    s.boot().await;
    s.stop().await;
    std::fs::remove_file(&key).unwrap();
    std::fs::write(&key, &saved).unwrap();
    std::fs::set_permissions(&key, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
    let err = litebucket::store::Store::open(load_config(&s.config_path))
        .unwrap_err()
        .to_string();
    assert!(err.contains("wrong master key"), "{err}");
    let cfg = load_config(&s.config_path);
    assert!(
        !litebucket::doctor::doctor(&cfg, false).unwrap(),
        "doctor reports it"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn offline_recover_creates_and_resets_admin_keys() {
    let (mut s, admin) = setup().await;
    admin.put("/docs/a", b"a").await;
    s.stop().await;
    let cfg = s.config_path.clone();
    // Lost master key: recovery without --reset-keys refuses.
    let key = s.dir.path().join("master.key");
    std::fs::remove_file(&key).unwrap();
    litebucket::secrets::MasterKey::generate(&key).unwrap();
    let out = std::process::Command::new(BIN)
        .args(["admin", "recover", "--config"])
        .arg(&cfg)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--reset-keys"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let keyfile = s.dir.path().join("recovered.env");
    let out = std::process::Command::new(BIN)
        .args([
            "admin",
            "recover",
            "--reset-keys",
            "--id",
            "rescue",
            "--config",
        ])
        .arg(&cfg)
        .arg("--output")
        .arg(&keyfile)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let env = std::fs::read_to_string(&keyfile).unwrap();
    assert_eq!(
        std::fs::metadata(&keyfile).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let secret = env
        .lines()
        .find_map(|l| l.strip_prefix("AWS_SECRET_ACCESS_KEY="))
        .unwrap()
        .to_string();
    s.boot().await;
    assert_eq!(
        s.admin().get("/docs/a", "").await.code(),
        "InvalidAccessKeyId"
    );
    let rescue = client_for(&s, "rescue", &secret);
    assert_eq!(rescue.get("/docs/a", "").await.body, b"a", "data untouched");
    // Recovery refuses while the server runs (store lock).
    let out = std::process::Command::new(BIN)
        .args(["admin", "recover", "--config"])
        .arg(&cfg)
        .output()
        .unwrap();
    assert!(!out.status.success());
}

use std::os::unix::fs::PermissionsExt;

#[tokio::test(flavor = "multi_thread")]
async fn cli_client_over_the_socket() {
    let (s, _) = setup().await;
    let socket = s.admin_socket();
    assert_eq!(
        std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let run = |args: &[&str]| {
        let socket = socket.clone();
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        async move {
            tokio::task::spawn_blocking(move || {
                std::process::Command::new(BIN)
                    .arg("admin")
                    .arg("--socket")
                    .arg(&socket)
                    .args(&args)
                    .output()
                    .unwrap()
            })
            .await
            .unwrap()
        }
    };
    let out = run(&[
        "key",
        "create",
        "--id",
        "cli-key",
        "--grant",
        "docs/in/:read,write",
        "--format",
        "env",
    ])
    .await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    let secret = text
        .lines()
        .find_map(|l| l.strip_prefix("AWS_SECRET_ACCESS_KEY="))
        .unwrap()
        .to_string();
    let k = client_for(&s, "cli-key", &secret);
    assert_eq!(k.put("/docs/in/x", b"x").await.status, 200);
    assert_eq!(k.put("/docs/out/x", b"x").await.code(), "AccessDenied");

    let out = run(&["--json", "key", "list"]).await;
    let list: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        list.as_array()
            .unwrap()
            .iter()
            .any(|k| k["access_key_id"] == "cli-key")
    );
    assert!(!String::from_utf8_lossy(&out.stdout).contains(&secret));

    let cors = s.dir.path().join("cors.xml");
    std::fs::write(
        &cors,
        "<CORSConfiguration><CORSRule><AllowedOrigin>*</AllowedOrigin><AllowedMethod>GET</AllowedMethod></CORSRule></CORSConfiguration>",
    )
    .unwrap();
    let out = run(&[
        "bucket",
        "create",
        "media",
        "--quota",
        "1G",
        "--cors-file",
        cors.to_str().unwrap(),
    ])
    .await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("quota:    1073741824"));
    let out = run(&["key", "disable", "cli-key"]).await;
    assert!(out.status.success());
    assert_eq!(k.get("/docs/in/x", "").await.code(), "InvalidAccessKeyId");
    let out = run(&["bucket", "delete", "nonexistent"]).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no bucket"));
}

#[tokio::test(flavor = "multi_thread")]
async fn cli_online_backup() {
    let (s, admin) = setup().await;
    admin.put("/docs/a", b"a").await;
    let socket = s.admin_socket();
    let dest = s.dir.path().join("nightly");
    let run = move |dest: std::path::PathBuf, json: bool| {
        let socket = socket.clone();
        tokio::task::spawn_blocking(move || {
            let mut cmd = std::process::Command::new(BIN);
            cmd.arg("admin").arg("--socket").arg(&socket);
            if json {
                cmd.arg("--json");
            }
            cmd.arg("backup").arg(&dest).output().unwrap()
        })
    };
    let out = run.clone()(dest.clone(), false).await.unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("backup complete: 1 files"), "{stdout}");
    assert!(dest.join("BACKUP_COMPLETE").exists());
    let out = run(s.dir.path().join("nightly-2"), true).await.unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["file_count"], 1);
}
