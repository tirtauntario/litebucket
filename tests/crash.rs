//! Process-crash and fault-injection tests (OPS-02, OPS-03, FS-03, FS-04,
//! OPS-07). Each case runs the real binary: seed state, restart with a
//! failpoint that aborts (SIGABRT, no cleanup) at a durable-state boundary,
//! then restart cleanly and verify that every acknowledged operation and
//! every untouched prior object survived and nothing partial is visible.
//!
//! Requires `--features failpoints`.
#![cfg(feature = "failpoints")]

mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::*;

const BIN: &str = env!("CARGO_BIN_EXE_storlite");

const PUT_POINTS: &[&str] = &[
    "after_commit:register",
    "create_staging",
    "write",
    "after_body_received",
    "after_file_sync",
    "before_publish",
    "after_publish_before_dir_sync",
    "after_dir_sync",
    "before_commit:object",
    "after_commit:object",
];

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

struct Node {
    dir: tempfile::TempDir,
    port: u16,
    mport: u16,
    runs: usize,
}

impl Node {
    fn new(extra: &str) -> Self {
        let extra = with_test_limits(extra);
        let dir = tempfile::tempdir().unwrap();
        let (port, mport) = (free_port(), free_port());
        let cfg = format!(
            r#"data_dir = "./data"
credentials_file = "./credentials.toml"
[http]
listen = "127.0.0.1:{port}"
allow_insecure_loopback_http = true
[management]
listen = "127.0.0.1:{mport}"
[logging]
format = "json"
level = "info"
{extra}
"#
        );
        std::fs::write(dir.path().join("config.toml"), cfg).unwrap();
        let creds = dir.path().join("credentials.toml");
        std::fs::write(&creds, CREDENTIALS).unwrap();
        std::fs::set_permissions(&creds, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
        let out = Command::new(BIN)
            .args(["init", "--config"])
            .arg(dir.path().join("config.toml"))
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        Self { dir, port, mport, runs: 0 }
    }

    fn config(&self) -> PathBuf {
        self.dir.path().join("config.toml")
    }

    fn log(&self, run: usize) -> PathBuf {
        self.dir.path().join(format!("serve-{run}.log"))
    }

    fn spawn(&mut self, env: &[(&str, &str)]) -> (Child, usize) {
        self.runs += 1;
        let run = self.runs;
        let log = std::fs::File::create(self.log(run)).unwrap();
        let mut cmd = Command::new(BIN);
        cmd.args(["serve", "--config"]).arg(self.config());
        cmd.env_remove("STORLITE_FAILPOINTS");
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.stdout(Stdio::null()).stderr(log).spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Ok(Some(status)) = child.try_wait() {
                panic!("server exited during startup ({status}): {}", std::fs::read_to_string(self.log(run)).unwrap());
            }
            if std::net::TcpStream::connect(("127.0.0.1", self.port)).is_ok()
                && readyz(self.mport).is_some_and(|s| s.contains("\"ready\":true"))
            {
                return (child, run);
            }
            assert!(Instant::now() < deadline, "server did not become ready");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn client(&self) -> Client {
        Client::new(&format!("http://127.0.0.1:{}", self.port), ADMIN)
    }

    fn data(&self) -> PathBuf {
        self.dir.path().join("data")
    }

    fn offline(&self, args: &[&str]) -> (bool, String) {
        let out = Command::new(BIN).args(args).arg("--config").arg(self.config()).output().unwrap();
        (
            out.status.success(),
            format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)),
        )
    }
}

fn readyz(port: u16) -> Option<String> {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    write!(s, "GET /readyz HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n").ok()?;
    let mut buf = String::new();
    s.read_to_string(&mut buf).ok()?;
    Some(buf)
}

fn stop(mut child: Child) {
    let pid = rustix::process::Pid::from_raw(child.id() as i32).unwrap();
    let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
    let deadline = Instant::now() + Duration::from_secs(20);
    while child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "server did not stop");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Wait for a crash; returns true if the process died abnormally.
fn wait_crash(child: &mut Child) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return !status.success();
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Offline verification after recovery: full check passes, GC reclaims all
/// tracked garbage, and no staging file remains.
fn verify_offline(node: &Node, ctx: &str) {
    let (ok, out) = node.offline(&["gc", "--apply"]);
    assert!(ok, "{ctx}: gc failed: {out}");
    let (ok, out) = node.offline(&["check", "--full"]);
    assert!(ok, "{ctx}: check --full failed:\n{out}");
    let staging = files_under(&node.data().join("staging"));
    assert!(staging.is_empty(), "{ctx}: staging files left: {staging:?}");
}

fn old_body() -> Vec<u8> {
    vec![b'o'; 300_000]
}

fn new_body() -> Vec<u8> {
    vec![b'n'; 300_000]
}

fn assert_whole(body: &[u8], ctx: &str) -> char {
    assert!(!body.is_empty(), "{ctx}: empty body");
    let first = body[0];
    assert!(body.iter().all(|b| *b == first), "{ctx}: partial or mixed object");
    assert_eq!(body.len(), 300_000, "{ctx}: wrong length");
    first as char
}

async fn crash_put(point: &str, overwrite: bool) {
    let ctx = format!("{} {point}", if overwrite { "overwrite" } else { "create" });
    let mut node = Node::new("");
    let (seed, _) = node.spawn(&[]);
    let c = node.client();
    assert_eq!(c.create_bucket("docs").await.status, 200);
    c.put("/docs/keep", b"untouched").await;
    if overwrite {
        assert_eq!(c.put("/docs/k", &old_body()).await.status, 200);
    }
    stop(seed);

    let fp = format!("{point}=abort");
    let (mut crashing, _) = node.spawn(&[("STORLITE_FAILPOINTS", fp.as_str())]);
    let r = c.put("/docs/k", &new_body()).await;
    assert!(wait_crash(&mut crashing), "{ctx}: failpoint not reached (status {})", r.status);
    assert_ne!(r.status, 200, "{ctx}: acknowledged despite crash");

    let (after, _) = node.spawn(&[]);
    assert_eq!(c.get("/docs/keep", "").await.body, b"untouched", "{ctx}");
    let g = c.get("/docs/k", "").await;
    match g.status {
        200 => {
            let v = assert_whole(&g.body, &ctx);
            if v == 'n' {
                // Only possible once the commit itself happened.
                assert!(point == "after_commit:object", "{ctx}: new object visible though crash preceded commit");
            }
        }
        404 => assert!(!overwrite, "{ctx}: previously committed object lost"),
        s => panic!("{ctx}: unexpected status {s}"),
    }
    if point == "after_commit:object" {
        assert_eq!(g.status, 200, "{ctx}");
        assert_eq!(g.body[0], b'n', "{ctx}: committed object lost");
    }
    // The store accepts writes again and listing is consistent.
    assert_eq!(c.put("/docs/k2", b"after").await.status, 200);
    stop(after);
    verify_offline(&node, &ctx);
}

#[tokio::test(flavor = "multi_thread")]
async fn ops_02_crash_matrix_new_objects() {
    for p in PUT_POINTS {
        crash_put(p, false).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn ops_02_crash_matrix_overwrites() {
    for p in PUT_POINTS {
        crash_put(p, true).await;
    }
}

async fn initiate(c: &Client, key: &str) -> String {
    c.send("POST", &format!("/docs/{key}"), "uploads", &[], Payload::Signed(vec![])).await.one("UploadId")
}

async fn upload_part(c: &Client, key: &str, id: &str, n: u32, body: &[u8]) -> Resp {
    c.send("PUT", &format!("/docs/{key}"), &format!("partNumber={n}&uploadId={id}"), &[], Payload::Signed(body.to_vec()))
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn ops_02_crash_matrix_part_replacement() {
    let points: Vec<String> = PUT_POINTS
        .iter()
        .map(|p| p.replace(":object", ":part"))
        .collect();
    for point in points.iter().map(String::as_str) {
        let ctx = format!("part {point}");
        let mut node = Node::new("");
        let (seed, _) = node.spawn(&[]);
        let c = node.client();
        c.create_bucket("docs").await;
        let id = initiate(&c, "mp").await;
        let old_etag = upload_part(&c, "mp", &id, 1, &old_body()).await.header("etag").unwrap();
        stop(seed);
        let fp = format!("{point}=abort");
        let (mut crashing, _) = node.spawn(&[("STORLITE_FAILPOINTS", fp.as_str())]);
        let r = upload_part(&c, "mp", &id, 1, &new_body()).await;
        assert!(wait_crash(&mut crashing), "{ctx}: not reached ({})", r.status);
        let (after, _) = node.spawn(&[]);
        let l = c.get("/docs/mp", &format!("uploadId={id}")).await;
        assert_eq!(l.status, 200, "{ctx}: upload lost: {}", l.text());
        let etag = l.one("ETag");
        if point != "after_commit:part" {
            assert_eq!(etag, old_etag, "{ctx}: part replaced without commit");
        }
        let body = format!(
            "<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>{etag}</ETag></Part></CompleteMultipartUpload>"
        );
        let r = c.send("POST", "/docs/mp", &format!("uploadId={id}"), &[], Payload::Signed(body.into_bytes())).await;
        assert_eq!(r.status, 200, "{ctx}: {}", r.text());
        let v = assert_whole(&c.get("/docs/mp", "").await.body, &ctx);
        assert_eq!(v == 'n', point == "after_commit:part", "{ctx}");
        stop(after);
        verify_offline(&node, &ctx);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn ops_02_crash_matrix_completion() {
    let points = [
        "after_commit:begin_completion",
        "during_assembly",
        "write",
        "after_file_sync",
        "after_publish_before_dir_sync",
        "after_dir_sync",
        "before_completion_commit",
        "before_commit:completion",
        "after_commit:completion",
    ];
    for point in points {
        let ctx = format!("complete {point}");
        let mut node = Node::new("");
        let (seed, _) = node.spawn(&[]);
        let c = node.client();
        c.create_bucket("docs").await;
        c.put("/docs/mp", &old_body()).await;
        let id = initiate(&c, "mp").await;
        let e = upload_part(&c, "mp", &id, 1, &new_body()).await.header("etag").unwrap();
        stop(seed);
        let body = format!(
            "<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>{e}</ETag></Part></CompleteMultipartUpload>"
        );
        let fp = format!("{point}=abort");
        let (mut crashing, _) = node.spawn(&[("STORLITE_FAILPOINTS", fp.as_str())]);
        let r = c
            .send("POST", "/docs/mp", &format!("uploadId={id}"), &[], Payload::Signed(body.clone().into_bytes()))
            .await;
        assert!(wait_crash(&mut crashing), "{ctx}: not reached ({})", r.status);
        let (after, _) = node.spawn(&[]);
        let v = assert_whole(&c.get("/docs/mp", "").await.body, &ctx);
        if point == "after_commit:completion" {
            assert_eq!(v, 'n', "{ctx}: committed completion lost");
            // Retry returns the receipt.
            let r = c.send("POST", "/docs/mp", &format!("uploadId={id}"), &[], Payload::Signed(body.into_bytes())).await;
            assert_eq!(r.status, 200, "{ctx}: receipt retry: {}", r.text());
        } else {
            assert_eq!(v, 'o', "{ctx}: partial completion visible");
            // Upload reopened with its committed part; completion succeeds now.
            let r = c.send("POST", "/docs/mp", &format!("uploadId={id}"), &[], Payload::Signed(body.into_bytes())).await;
            assert_eq!(r.status, 200, "{ctx}: retry after recovery: {}", r.text());
            assert_eq!(assert_whole(&c.get("/docs/mp", "").await.body, &ctx), 'n');
        }
        stop(after);
        verify_offline(&node, &ctx);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn ops_02_crash_matrix_delete_and_gc() {
    for point in ["before_commit:delete", "after_commit:delete"] {
        let ctx = format!("delete {point}");
        let mut node = Node::new("");
        let (seed, _) = node.spawn(&[]);
        let c = node.client();
        c.create_bucket("docs").await;
        c.put("/docs/k", &old_body()).await;
        stop(seed);
        let fp = format!("{point}=abort");
        let (mut crashing, _) = node.spawn(&[("STORLITE_FAILPOINTS", fp.as_str())]);
        let _ = c.delete("/docs/k").await;
        assert!(wait_crash(&mut crashing), "{ctx}: not reached");
        let (after, _) = node.spawn(&[]);
        let g = c.get("/docs/k", "").await;
        if point == "after_commit:delete" {
            assert_eq!(g.status, 404, "{ctx}");
        } else {
            assert_whole(&g.body, &ctx);
        }
        stop(after);
        verify_offline(&node, &ctx);
    }
    for point in ["gc_before_unlink", "gc_after_unlink"] {
        let ctx = format!("gc {point}");
        let extra = "[maintenance]\ngarbage_grace_seconds = 0\ngarbage_interval_seconds = 1\n";
        let mut node = Node::new(extra);
        let (seed, _) = node.spawn(&[]);
        let c = node.client();
        c.create_bucket("docs").await;
        c.put("/docs/k", &old_body()).await;
        c.put("/docs/k", &new_body()).await;
        c.put("/docs/gone", b"x").await;
        c.delete("/docs/gone").await;
        stop(seed);
        let fp = format!("{point}=abort");
        let (mut crashing, _) = node.spawn(&[("STORLITE_FAILPOINTS", fp.as_str())]);
        assert!(wait_crash(&mut crashing), "{ctx}: collector did not run");
        let (after, _) = node.spawn(&[]);
        assert_eq!(assert_whole(&c.get("/docs/k", "").await.body, &ctx), 'n');
        stop(after);
        verify_offline(&node, &ctx);
        assert_eq!(files_under(&node.data().join("objects")).len(), 1, "{ctx}: garbage replayed idempotently");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn fs_04_injected_io_failures_never_acknowledge() {
    // fsync failure (EIO): no acknowledgment, mutations halt until restart.
    let mut node = Node::new("");
    let (seed, _) = node.spawn(&[]);
    let c = node.client();
    c.create_bucket("docs").await;
    c.put("/docs/k", &old_body()).await;
    stop(seed);
    for (fp, want_status) in [("sync=eio", 500u16), ("sync_dir=eio", 500), ("write=enospc", 503), ("commit:object=eio", 500)] {
        let (child, _) = node.spawn(&[("STORLITE_FAILPOINTS", fp)]);
        let r = c.put("/docs/k", &new_body()).await;
        assert_eq!(r.status, want_status, "{fp}: {}", r.text());
        assert_eq!(assert_whole(&c.get("/docs/k", "").await.body, fp), 'o', "{fp}: old object replaced");
        if fp.contains("eio") && !fp.starts_with("commit") {
            let rz = readyz(node.mport).unwrap();
            assert!(rz.contains("\"mutations_halted\":true"), "{fp}: {rz}");
            assert_eq!(c.put("/docs/other", b"x").await.status, 503, "{fp}: writes refused while halted");
        }
        stop(child);
    }
    let (child, _) = node.spawn(&[]);
    assert_eq!(c.put("/docs/k", &new_body()).await.status, 200);
    stop(child);
    verify_offline(&node, "io failures");
}

#[tokio::test(flavor = "multi_thread")]
async fn fs_03_injected_storage_id_collisions_preserve_existing_files() {
    let mut node = Node::new("");
    let (seed, _) = node.spawn(&[]);
    let c = node.client();
    c.create_bucket("docs").await;
    c.put("/docs/a", &old_body()).await;
    stop(seed);
    let existing = files_under(&node.data().join("objects"))[0].clone();
    let id = existing.file_name().unwrap().to_string_lossy().to_string();
    // Untracked file squatting on another ID's path.
    let squat_id = "ab".to_string() + &"0".repeat(30);
    let squat_dir = node.data().join("objects/ab/00");
    std::fs::create_dir_all(&squat_dir).unwrap();
    std::fs::write(squat_dir.join(&squat_id), b"squatter").unwrap();
    let forced = format!("{id},{squat_id}");
    let (child, _) = node.spawn(&[("STORLITE_FORCE_STORAGE_IDS", forced.as_str())]);
    let r = c.put("/docs/b", &new_body()).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let r = c.put("/docs/c", b"third").await;
    assert_eq!(r.status, 200);
    assert_eq!(assert_whole(&c.get("/docs/a", "").await.body, "a"), 'o');
    assert_eq!(assert_whole(&c.get("/docs/b", "").await.body, "b"), 'n');
    assert_eq!(std::fs::read(squat_dir.join(&squat_id)).unwrap(), b"squatter");
    stop(child);
    let (ok, out) = node.offline(&["check", "--full"]);
    assert!(!ok && out.contains("untracked"), "squatter reported, not deleted:\n{out}");
    assert!(squat_dir.join(&squat_id).exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn ops_07_logs_exclude_secrets_and_keys() {
    let mut node = Node::new("");
    let (child, run) = node.spawn(&[]);
    let c = node.client();
    c.create_bucket("docs").await;
    c.put_h("/docs/very-secret-key-name.pdf", &[("x-amz-meta-ssn", "123-45-6789")], b"body").await;
    let url = c.presign("GET", "/docs/very-secret-key-name.pdf", 300, None);
    let sig = url.split("X-Amz-Signature=").nth(1).unwrap().to_string();
    raw("GET", &url, &[], vec![]).await;
    Client::new(&c.base, ("admin-key", "wrongwrongwrongwrongwrongwrongwrong")).get("/docs/x", "").await;
    stop(child);
    let log = std::fs::read_to_string(node.log(run)).unwrap();
    assert!(log.contains("\"operation\":\"PutObject\""), "{log}");
    for secret in [ADMIN.1, APP.1, sig.as_str(), "very-secret-key-name", "123-45-6789", "Signature=", "X-Amz-Credential"] {
        assert!(!log.contains(secret), "log leaks {secret}");
    }
    let _ = Path::new("");
}
