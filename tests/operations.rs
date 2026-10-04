//! Operational behavior: locking, GC safety, integrity faults, backup and
//! restore, capacity/overload, slow clients (OPS-*, CAP-*, GET-03, DB-02).

mod common;

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use common::*;
use storlite::capacity::PermitKind;
use storlite::metadata::queries;

async fn setup_with(extra: &str) -> (TestServer, Client) {
    let s = TestServer::start_with(extra).await;
    let c = s.admin();
    assert_eq!(c.create_bucket("docs").await.status, 200);
    (s, c)
}

#[tokio::test(flavor = "multi_thread")]
async fn ops_01_second_owner_is_refused_and_lock_is_kept() {
    let (s, _c) = setup_with("").await;
    let cfg = load_config(&s.config_path);
    let lock = s.data_dir().join("store.lock");
    let ino = std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(&lock).unwrap());
    let err = storlite::store::open_offline(&cfg, false).unwrap_err();
    assert!(matches!(err, storlite::error::Error::Locked), "{err}");
    assert!(storlite::store::Store::open(cfg.clone()).is_err());
    assert!(storlite::doctor::gc(&cfg, true).is_err());
    assert!(
        storlite::store::initialize(&cfg).is_err(),
        "init refuses a non-empty directory"
    );
    let ino2 = std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(&lock).unwrap());
    assert_eq!(ino, ino2, "lock file never replaced");
}

#[tokio::test(flavor = "multi_thread")]
async fn ops_04_gc_reclaims_only_tracked_eligible_garbage() {
    let (s, c) = setup_with("[maintenance]\ngarbage_grace_seconds = 0\n").await;
    let store = s.store();
    c.put("/docs/live", b"live").await;
    c.put("/docs/old", b"v1").await;
    c.put("/docs/old", b"v2").await;
    c.put("/docs/gone", b"x").await;
    c.delete("/docs/gone").await;
    // An unknown file placed by hand in a shard directory.
    let stray_dir = s.data_dir().join("objects/ab/cd");
    std::fs::create_dir_all(&stray_dir).unwrap();
    let stray = stray_dir.join("abcd0000000000000000000000000000");
    std::fs::write(&stray, b"unknown").unwrap();
    let before = files_under(&s.data_dir().join("objects")).len();
    assert_eq!(before, 5);
    while storlite::maintenance::gc_once(&store).await.unwrap() > 0 {}
    let after = files_under(&s.data_dir().join("objects")).len();
    assert_eq!(
        after, 3,
        "two garbage files removed; live, current, and stray kept"
    );
    assert!(
        stray.exists(),
        "untracked files are never deleted automatically"
    );
    assert_eq!(c.get("/docs/live", "").await.body, b"live");
    assert_eq!(c.get("/docs/old", "").await.body, b"v2");
    // Garbage still within its grace period is not collected.
    let s2 = TestServer::start_with("[maintenance]\ngarbage_grace_seconds = 3600\n").await;
    let c2 = s2.admin();
    c2.create_bucket("docs").await;
    c2.put("/docs/a", b"1").await;
    c2.put("/docs/a", b"2").await;
    assert_eq!(
        storlite::maintenance::gc_once(&s2.store()).await.unwrap(),
        0
    );
    assert_eq!(files_under(&s2.data_dir().join("objects")).len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn ops_06_missing_or_corrupt_files_are_integrity_failures() {
    let (s, c) = setup_with("").await;
    c.put("/docs/a", b"aaaa").await;
    c.put("/docs/b", b"bbbb").await;
    let files = files_under(&s.data_dir().join("objects"));
    let store = s.store();
    let (b, k) = (
        store
            .db
            .read(|c| queries::bucket_by_name(c, "docs"))
            .await
            .unwrap()
            .unwrap()
            .id,
        b"a".to_vec(),
    );
    let row = store
        .db
        .read(move |c| queries::get_object(c, &b, &k))
        .await
        .unwrap()
        .unwrap();
    let path = files
        .iter()
        .find(|p| p.ends_with(row.storage_id.to_hex()))
        .unwrap()
        .clone();
    std::fs::remove_file(&path).unwrap();
    let r = c.get("/docs/a", "").await;
    assert_eq!(
        (r.status, r.code().as_str()),
        (500, "InternalError"),
        "not NoSuchKey"
    );
    let ready = raw("GET", &format!("{}/readyz", s.mgmt), &[], vec![]).await;
    assert_eq!(ready.status, 503);
    assert!(ready.text().contains("integrity_failure"));
    // Metadata is never removed automatically.
    assert_eq!(c.head("/docs/a").await.status, 200);
    // Truncated file.
    let other = files.iter().find(|p| **p != path).unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(other)
        .unwrap()
        .set_len(2)
        .unwrap();
    assert_eq!(c.get("/docs/b", "").await.status, 500);
}

#[tokio::test(flavor = "multi_thread")]
async fn ops_05_backup_and_restore_round_trip() {
    let (mut s, c) = setup_with("").await;
    c.create_bucket("other").await;
    let big: Vec<u8> = (0..1_500_000u32).map(|i| (i % 253) as u8).collect();
    c.put_h(
        "/docs/a.txt",
        &[("content-type", "text/plain"), ("x-amz-meta-k", "v")],
        b"alpha",
    )
    .await;
    c.put("/docs/dir/big.bin", &big).await;
    c.put("/other/x", b"x").await;
    c.put("/docs/replaced", b"old").await;
    c.put("/docs/replaced", b"new").await;
    let up = c
        .send("POST", "/docs/mp", "uploads", &[], Payload::Signed(vec![]))
        .await
        .one("UploadId");
    let p1 = c
        .send(
            "PUT",
            "/docs/mp",
            &format!("partNumber=1&uploadId={up}"),
            &[],
            Payload::Signed(vec![7u8; 5 * 1024 * 1024]),
        )
        .await
        .header("etag")
        .unwrap();
    let listing = c.get("/docs", "list-type=2").await.text();
    let cfg = load_config(&s.config_path);
    s.stop().await;

    let backup_dir = s.dir.path().join("backup-1");
    storlite::backup::backup(&cfg, &backup_dir).unwrap();
    assert!(backup_dir.join("BACKUP_COMPLETE").exists());
    assert!(!backup_dir.join("BACKUP_INCOMPLETE").exists());
    let mode = std::os::unix::fs::PermissionsExt::mode(
        &std::fs::metadata(&backup_dir).unwrap().permissions(),
    );
    assert_eq!(mode & 0o777, 0o700);
    // Backing up into an existing directory is refused.
    assert!(storlite::backup::backup(&cfg, &backup_dir).is_err());

    // Restore into a different empty directory and serve from it.
    let restored = s.dir.path().join("restored");
    let key = s.dir.path().join("master.key");
    // Encrypted access keys need the master key to be verified.
    assert!(
        storlite::backup::restore(&backup_dir, &s.dir.path().join("nokey"), None, false).is_err()
    );
    let other_key = s.dir.path().join("other.key");
    storlite::secrets::MasterKey::generate(&other_key).unwrap();
    let err = storlite::backup::restore(
        &backup_dir,
        &s.dir.path().join("wrongkey"),
        Some(&other_key),
        false,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("master key does not open"), "{err}");
    assert!(
        !s.dir.path().join("wrongkey").exists(),
        "nothing written before the key check"
    );
    storlite::backup::restore(&backup_dir, &restored, Some(&key), false).unwrap();
    assert!(
        storlite::backup::restore(&backup_dir, &restored, Some(&key), false).is_err(),
        "non-empty target refused"
    );
    let text = std::fs::read_to_string(&s.config_path)
        .unwrap()
        .replace("data_dir = \"./data\"", "data_dir = \"./restored\"");
    std::fs::write(&s.config_path, text).unwrap();
    s.boot().await;
    let c = s.admin();
    assert_eq!(
        c.get("/docs", "list-type=2")
            .await
            .text()
            .replace(|ch: char| ch.is_ascii_digit(), ""),
        listing.replace(|ch: char| ch.is_ascii_digit(), "")
    );
    let g = c.get("/docs/a.txt", "").await;
    assert_eq!(g.body, b"alpha");
    assert_eq!(g.header("x-amz-meta-k").unwrap(), "v");
    assert_eq!(c.get("/docs/dir/big.bin", "").await.body, big);
    assert_eq!(c.get("/docs/replaced", "").await.body, b"new");
    assert_eq!(c.get("/other/x", "").await.body, b"x");
    // The restored multipart upload can continue and complete.
    let p2 = c
        .send(
            "PUT",
            "/docs/mp",
            &format!("partNumber=2&uploadId={up}"),
            &[],
            Payload::Signed(b"end".to_vec()),
        )
        .await
        .header("etag")
        .unwrap();
    let body = format!(
        "<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>{p1}</ETag></Part><Part><PartNumber>2</PartNumber><ETag>{p2}</ETag></Part></CompleteMultipartUpload>"
    );
    let r = c
        .send(
            "POST",
            "/docs/mp",
            &format!("uploadId={up}"),
            &[],
            Payload::Signed(body.into_bytes()),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(
        c.head("/docs/mp").await.header("content-length").unwrap(),
        (5 * 1024 * 1024 + 3).to_string()
    );

    // A corrupted backup file fails restore.
    s.stop().await;
    let victim = files_under(&backup_dir.join("objects"))[0].clone();
    let mut bytes = std::fs::read(&victim).unwrap();
    bytes[0] ^= 0xff;
    std::fs::write(&victim, bytes).unwrap();
    assert!(
        storlite::backup::restore(
            &backup_dir,
            &s.dir.path().join("restored2"),
            Some(&key),
            false
        )
        .is_err()
    );
    // An incomplete backup is refused.
    std::fs::remove_file(backup_dir.join("BACKUP_COMPLETE")).unwrap();
    assert!(
        storlite::backup::restore(
            &backup_dir,
            &s.dir.path().join("restored3"),
            Some(&key),
            false
        )
        .is_err()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn doctor_reports_consistency_and_untracked_files() {
    let (mut s, c) = setup_with("").await;
    c.put("/docs/a", b"a").await;
    let cfg = load_config(&s.config_path);
    s.stop().await;
    assert!(storlite::doctor::doctor(&cfg, true).unwrap());
    std::fs::create_dir_all(s.data_dir().join("staging/00/11")).unwrap();
    std::fs::write(s.data_dir().join("staging/00/11/junk.tmp"), b"j").unwrap();
    assert!(
        !storlite::doctor::doctor(&cfg, true).unwrap(),
        "untracked file reported"
    );
    assert!(
        s.data_dir().join("staging/00/11/junk.tmp").exists(),
        "doctor never deletes"
    );
    storlite::doctor::gc(&cfg, true).unwrap();
    assert!(
        s.data_dir().join("staging/00/11/junk.tmp").exists(),
        "gc never deletes untracked files"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn cap_04_counters_through_overwrite_delete_restart() {
    let (mut s, c) = setup_with("").await;
    c.put("/docs/a", &[1u8; 100]).await;
    c.put("/docs/a", &[1u8; 40]).await;
    c.put("/docs/b", &[1u8; 7]).await;
    c.put_h(
        "/docs/b",
        &[("content-md5", "1B2M2Y8AsgTpgAmY7PhCfg==")],
        &[1u8; 9],
    )
    .await; // rejected
    c.delete("/docs/missing").await;
    s.restart().await;
    let store = s.store();
    let row = store
        .db
        .read(|c| queries::bucket_by_name(c, "docs"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!((row.object_count, row.logical_bytes), (2, 47));
    assert!(
        store
            .db
            .read(|c| queries::invariant_violations(c))
            .await
            .unwrap()
            .is_empty()
    );
}

fn tcp_connect(base: &str) -> std::net::TcpStream {
    let s = std::net::TcpStream::connect(base.trim_start_matches("http://")).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s
}

#[tokio::test(flavor = "multi_thread")]
async fn db_02_slow_upload_holds_no_metadata_transaction() {
    let (s, c) = setup_with("").await;
    let head = c.signed_head_unsigned_payload("PUT", "/docs/slow", 1_000_000);
    let base = s.base.clone();
    let slow = std::thread::spawn(move || {
        let mut t = tcp_connect(&base);
        t.write_all(head.as_bytes()).unwrap();
        t.write_all(&[1u8; 1000]).unwrap();
        std::thread::sleep(Duration::from_millis(800));
        t.write_all(&vec![1u8; 999_000]).unwrap();
        let mut buf = [0u8; 12];
        t.read_exact(&mut buf).unwrap();
        String::from_utf8_lossy(&buf).to_string()
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    // While the slow body is in flight, other writes and reads proceed.
    let started = Instant::now();
    assert_eq!(c.put("/docs/fast", b"fast").await.status, 200);
    assert_eq!(c.get("/docs/fast", "").await.status, 200);
    c.create_bucket("another").await;
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "writer not blocked by the slow upload"
    );
    assert!(slow.join().unwrap().contains("200"));
    assert_eq!(c.get("/docs/slow", "").await.body.len(), 1_000_000);
}

#[tokio::test(flavor = "multi_thread")]
async fn cap_02_concurrent_uploads_cannot_over_admit_temporary_space() {
    let (s, c) = setup_with(
        "[limits]\nmax_temporary_bytes = 6000000\nmax_single_put_bytes = 5000000\nmax_part_bytes = 5242880\nmax_object_bytes = 5000000\n",
    )
    .await;
    let head = c.signed_head_unsigned_payload("PUT", "/docs/first", 4_000_000);
    let base = s.base.clone();
    let first = std::thread::spawn(move || {
        let mut t = tcp_connect(&base);
        t.write_all(head.as_bytes()).unwrap();
        t.write_all(&[1u8; 10]).unwrap();
        std::thread::sleep(Duration::from_millis(600));
        t.write_all(&vec![1u8; 4_000_000 - 10]).unwrap();
        let mut buf = [0u8; 12];
        t.read_exact(&mut buf).unwrap();
        String::from_utf8_lossy(&buf).to_string()
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let r = c.put("/docs/second", &vec![2u8; 4_000_000]).await;
    // 503 before the body is read; the client may also see the early close.
    assert!(
        r.status == 503 || r.status == 0,
        "{}: {}",
        r.status,
        r.text()
    );
    assert_eq!(c.head("/docs/second").await.status, 404);
    assert!(first.join().unwrap().contains("200"));
    // Reservation released: the next upload fits.
    assert_eq!(
        c.put("/docs/third", &vec![3u8; 4_000_000]).await.status,
        200
    );
    assert_eq!(s.store().capacity.reserved_bytes(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn cap_01_get_03_overload_and_abandoned_downloads_are_bounded() {
    let (s, c) = setup_with("[limits]\nactive_downloads = 1\nadmission_timeout_ms = 100\ntransfer_buffer_bytes = 4096\n").await;
    c.put("/docs/big", &vec![9u8; 8 * 1024 * 1024]).await;
    let store = s.store();
    // A reader that stops reading holds the single download permit.
    let url = c.presign("GET", "/docs/big", 60, None);
    let path_q = url.trim_start_matches(&s.base).to_string();
    let base = s.base.clone();
    let mut stalled = tcp_connect(&base);
    write!(
        stalled,
        "GET {path_q} HTTP/1.1\r\nhost: {}\r\n\r\n",
        base.trim_start_matches("http://")
    )
    .unwrap();
    let mut first = [0u8; 1024];
    let _ = stalled.read(&mut first).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(store.capacity.in_use(PermitKind::Download), 1);
    let r = c.get("/docs/big", "").await;
    assert_eq!((r.status, r.code().as_str()), (503, "SlowDown"));
    // HEAD needs no download permit.
    assert_eq!(c.head("/docs/big").await.status, 200);
    // Disconnecting releases the permit and the file handle.
    drop(stalled);
    let deadline = Instant::now() + Duration::from_secs(5);
    while store.capacity.in_use(PermitKind::Download) != 0 {
        assert!(
            Instant::now() < deadline,
            "permit not released after disconnect"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(c.get("/docs/big", "").await.body.len(), 8 * 1024 * 1024);
    // Overwrite/delete while an old file is open: the old reader is safe.
    let mut reader = tcp_connect(&base);
    write!(
        reader,
        "GET {path_q} HTTP/1.1\r\nhost: {}\r\nconnection: close\r\n\r\n",
        base.trim_start_matches("http://")
    )
    .unwrap();
    let mut got = Vec::new();
    let mut buf = vec![0u8; 65536];
    let n = reader.read(&mut buf).unwrap();
    got.extend_from_slice(&buf[..n]);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let _ = c.delete("/docs/big").await;
    while storlite::maintenance::gc_once(&store).await.unwrap() > 0 {}
    reader.read_to_end(&mut got).unwrap();
    let body_start = got.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    assert_eq!(got.len() - body_start, 8 * 1024 * 1024);
    assert!(got[body_start..].iter().all(|b| *b == 9));
}

#[tokio::test(flavor = "multi_thread")]
async fn graceful_shutdown_drains_and_releases_lock() {
    let (mut s, c) = setup_with("").await;
    c.put("/docs/a", b"a").await;
    s.stop().await;
    let cfg = load_config(&s.config_path);
    // Lock released after shutdown: offline commands can run.
    let (_d, _conn, _m, _) = storlite::store::open_offline(&cfg, false).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn http_03_idle_body_times_out_and_preserves_object() {
    let (s, c) = setup_with("[http]\nbody_idle_timeout_seconds = 1\nallow_insecure_loopback_http = true\nlisten = \"127.0.0.1:0\"\n").await;
    c.put("/docs/k", b"original").await;
    let head = c.signed_head_unsigned_payload("PUT", "/docs/k", 1000);
    let mut t = tcp_connect(&s.base);
    t.write_all(head.as_bytes()).unwrap();
    t.write_all(&[1u8; 10]).unwrap();
    // Stall longer than the idle timeout.
    let mut resp = String::new();
    let _ = t.read_to_string(&mut resp);
    assert!(resp.starts_with("HTTP/1.1 400"), "{resp}");
    assert!(resp.contains("RequestTimeout"));
    assert_eq!(c.get("/docs/k", "").await.body, b"original");
    assert_eq!(s.store().capacity.reserved_bytes(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn trusted_proxy_mode_refuses_other_peers() {
    let s = TestServer::start_with(
        "[http]\nlisten = \"127.0.0.1:0\"\ntrusted_proxy_mode = true\ntrusted_proxy_addresses = [\"10.9.8.7\"]\n",
    )
    .await;
    // 127.0.0.1 is not the configured proxy: the connection is closed.
    let mut t = tcp_connect(&s.base);
    let _ = write!(t, "GET / HTTP/1.1\r\nhost: x\r\n\r\n");
    let mut buf = Vec::new();
    let n = t.read_to_end(&mut buf).unwrap_or(0);
    assert_eq!(n, 0, "no response to a non-proxy peer");
}

#[tokio::test(flavor = "multi_thread")]
async fn db_04_missing_or_newer_metadata_never_opens_an_empty_store() {
    let (mut s, c) = setup_with("").await;
    c.put("/docs/a", b"a").await;
    s.stop().await;
    let cfg = load_config(&s.config_path);
    // A newer schema version is refused, and nothing is modified.
    {
        let conn = rusqlite::Connection::open(s.data_dir().join("metadata.sqlite3")).unwrap();
        conn.execute(
            "INSERT INTO schema_migrations VALUES (999, 'future', zeroblob(32), 0)",
            [],
        )
        .unwrap();
    }
    assert!(storlite::store::Store::open(cfg.clone()).is_err());
    {
        let conn = rusqlite::Connection::open(s.data_dir().join("metadata.sqlite3")).unwrap();
        conn.execute("DELETE FROM schema_migrations WHERE version = 999", [])
            .unwrap();
    }
    // A missing database in a non-empty data directory is never recreated.
    let db = s.data_dir().join("metadata.sqlite3");
    let moved = s.data_dir().join("moved.sqlite3");
    std::fs::rename(&db, &moved).unwrap();
    let _ = std::fs::remove_file(s.data_dir().join("metadata.sqlite3-wal"));
    let _ = std::fs::remove_file(s.data_dir().join("metadata.sqlite3-shm"));
    let err = storlite::store::Store::open(cfg.clone())
        .unwrap_err()
        .to_string();
    assert!(err.contains("refusing to create an empty store"), "{err}");
    assert!(!db.exists());
    std::fs::rename(&moved, &db).unwrap();
    s.boot().await;
    assert_eq!(s.admin().get("/docs/a", "").await.body, b"a");
}
