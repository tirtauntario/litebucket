#!/usr/bin/env python3
"""Resource and performance evidence (PERF-01, CAP-01, 19.4).

Starts the release binary on a temporary store with production durability
settings (WAL + synchronous=FULL, file and directory fsync, checksums, SigV4)
and measures: idle RSS, peak RSS/FDs during large and concurrent transfers,
small-object PUT/GET rates, listing latency at increasing key counts, WAL
size, and multipart assembly time. Results are printed as JSON.

Usage: .interop/venv/bin/python scripts/bench.py [--large-mib 1024]
Requires boto3 (see scripts/interop.sh). Local endpoint only.
"""

import argparse
import concurrent.futures as cf
import json
import os
import platform
import shutil
import socket
import statistics
import subprocess
import tempfile
import threading
import time
import urllib.request

import boto3
from boto3.s3.transfer import TransferConfig
from botocore.config import Config

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.environ.get("STORLITE_BIN", os.path.join(ROOT, "target/release/storlite"))
MIB = 1024 * 1024


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


class Sampler:
    """Samples RSS (KiB) and open FDs of a process."""

    def __init__(self, pid):
        self.pid, self.peak_rss, self.peak_fds, self.stop = pid, 0, 0, False
        self.t = threading.Thread(target=self.run, daemon=True)

    def rss(self):
        out = subprocess.run(["ps", "-o", "rss=", "-p", str(self.pid)], capture_output=True, text=True).stdout
        return int(out.strip() or 0)

    def fds(self):
        out = subprocess.run(["lsof", "-p", str(self.pid)], capture_output=True, text=True).stdout
        return max(0, len(out.splitlines()) - 1)

    def run(self):
        i = 0
        while not self.stop:
            self.peak_rss = max(self.peak_rss, self.rss())
            if i % 10 == 0:
                self.peak_fds = max(self.peak_fds, self.fds())
            i += 1
            time.sleep(0.05)

    def __enter__(self):
        self.peak_rss, self.peak_fds = 0, 0
        self.stop = False
        self.t = threading.Thread(target=self.run, daemon=True)
        self.t.start()
        return self

    def __exit__(self, *a):
        self.stop = True
        self.t.join()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--large-mib", type=int, default=1024)
    ap.add_argument("--small-count", type=int, default=2000)
    args = ap.parse_args()
    work = tempfile.mkdtemp(prefix="storlite-bench-")
    port, mport = free_port(), free_port()
    with open(os.path.join(work, "config.toml"), "w") as f:
        f.write(f"""data_dir = "./data"
credentials_file = "./credentials.toml"
[http]
listen = "127.0.0.1:{port}"
allow_insecure_loopback_http = true
[management]
listen = "127.0.0.1:{mport}"
[limits]
min_disk_free_bytes = 268435456
min_disk_free_percent = 0
[logging]
level = "warn"
""")
    creds = os.path.join(work, "credentials.toml")
    with open(creds, "w") as f:
        f.write('[[credentials]]\nid = "bench"\nsecret_access_key = "benchsecretbenchsecretbenchsecret01"\nenabled = true\nglobal_grants = ["admin"]\n')
    os.chmod(creds, 0o600)
    cfg = os.path.join(work, "config.toml")
    subprocess.run([BIN, "init", "--config", cfg], check=True, capture_output=True)
    server = subprocess.Popen([BIN, "serve", "--config", cfg], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(200):
        try:
            urllib.request.urlopen(f"http://127.0.0.1:{mport}/readyz").read()
            break
        except Exception:
            time.sleep(0.05)
    endpoint = f"http://127.0.0.1:{port}"

    def client():
        # One session per client: boto3's default session is not thread-safe.
        return boto3.session.Session().client(
            "s3", endpoint_url=endpoint, region_name="us-east-1",
            aws_access_key_id="bench", aws_secret_access_key="benchsecretbenchsecretbenchsecret01",
            config=Config(signature_version="s3v4", s3={"addressing_style": "path"}, max_pool_connections=32),
        )

    s3 = client()
    s3.create_bucket(Bucket="bench")
    sampler = Sampler(server.pid)
    res = {
        "host": {"os": platform.platform(), "machine": platform.machine(), "cpus": os.cpu_count()},
        "binary_bytes": os.path.getsize(BIN),
        "idle_rss_kib": sampler.rss(),
        "idle_fds": sampler.fds(),
    }

    # Large single-stream transfer: memory must not track object size.
    big = os.path.join(work, "big.bin")
    with open(big, "wb") as f:
        chunk = os.urandom(MIB)
        for _ in range(args.large_mib):
            f.write(chunk)
    with sampler:
        t = time.time()
        with open(big, "rb") as f:
            s3.put_object(Bucket="bench", Key="big", Body=f, ContentLength=args.large_mib * MIB)
        put_s = time.time() - t
    res["large_put"] = {"mib": args.large_mib, "seconds": round(put_s, 2), "mib_per_s": round(args.large_mib / put_s, 1), "peak_rss_kib": sampler.peak_rss, "peak_fds": sampler.peak_fds}
    with sampler:
        t = time.time()
        body = s3.get_object(Bucket="bench", Key="big")["Body"]
        n = 0
        for c in body.iter_chunks(1 * MIB):
            n += len(c)
        get_s = time.time() - t
    res["large_get"] = {"mib": n // MIB, "seconds": round(get_s, 2), "mib_per_s": round(n / MIB / get_s, 1), "peak_rss_kib": sampler.peak_rss, "peak_fds": sampler.peak_fds}

    # Concurrent large transfers (bounded by active upload/download permits).
    with sampler:
        t = time.time()
        with cf.ThreadPoolExecutor(8) as ex:
            list(ex.map(lambda i: client().put_object(Bucket="bench", Key=f"par/{i}", Body=os.urandom(64 * MIB)), range(8)))
        res["concurrent_put_8x64mib"] = {"seconds": round(time.time() - t, 2), "peak_rss_kib": sampler.peak_rss, "peak_fds": sampler.peak_fds}
    with sampler:
        t = time.time()
        with cf.ThreadPoolExecutor(8) as ex:
            list(ex.map(lambda i: len(client().get_object(Bucket="bench", Key=f"par/{i}")["Body"].read()), range(8)))
        res["concurrent_get_8x64mib"] = {"seconds": round(time.time() - t, 2), "peak_rss_kib": sampler.peak_rss, "peak_fds": sampler.peak_fds}

    # Multipart upload + assembly of the large file with 64 MiB parts.
    with sampler:
        t = time.time()
        s3.upload_file(big, "bench", "mp", Config=TransferConfig(multipart_threshold=64 * MIB, multipart_chunksize=64 * MIB, max_concurrency=4))
        res["multipart_upload"] = {"mib": args.large_mib, "seconds": round(time.time() - t, 2), "peak_rss_kib": sampler.peak_rss, "peak_fds": sampler.peak_fds}
    os.remove(big)

    # Small-object metadata contention: many 1 KiB PUTs/GETs from 16 threads.
    payload = os.urandom(1024)
    clients = [client() for _ in range(16)]

    def timed(fn):
        t = time.perf_counter()
        fn()
        return time.perf_counter() - t

    def run_small(op):
        lat = []
        with cf.ThreadPoolExecutor(16) as ex:
            futs = [
                ex.submit(timed, (lambda i=i: clients[i % 16].put_object(Bucket="bench", Key=f"small/{i:06d}", Body=payload))
                if op == "put" else (lambda i=i: clients[i % 16].get_object(Bucket="bench", Key=f"small/{i:06d}")["Body"].read()))
                for i in range(args.small_count)
            ]
            t = time.time()
            lat = [f.result() for f in futs]
            wall = time.time() - t
        lat.sort()
        return {"ops": args.small_count, "ops_per_s": round(args.small_count / wall, 1),
                "p50_ms": round(statistics.median(lat) * 1000, 2), "p99_ms": round(lat[int(len(lat) * 0.99) - 1] * 1000, 2)}

    with sampler:
        res["small_put_1kib_16threads"] = run_small("put")
        res["small_put_1kib_16threads"]["peak_rss_kib"] = sampler.peak_rss
    res["small_get_1kib_16threads"] = run_small("get")

    # Listing latency as the key count grows (one 1000-key page; delimiter).
    listing = {}
    total = args.small_count
    for target in [total, 10_000, 30_000]:
        if target > total:
            with cf.ThreadPoolExecutor(16) as ex:
                list(ex.map(lambda i: clients[i % 16].put_object(Bucket="bench", Key=f"small/{i:06d}", Body=b"x"), range(total, target)))
            total = target
        t = time.perf_counter()
        for _ in range(10):
            s3.list_objects_v2(Bucket="bench", Prefix="small/", MaxKeys=1000)
        page = (time.perf_counter() - t) / 10
        t = time.perf_counter()
        for _ in range(10):
            s3.list_objects_v2(Bucket="bench", Delimiter="/")
        rolled = (time.perf_counter() - t) / 10
        listing[str(target)] = {"page_1000_ms": round(page * 1000, 2), "delimiter_rollup_ms": round(rolled * 1000, 2)}
    res["listing_latency"] = listing
    wal = os.path.join(work, "data", "metadata.sqlite3-wal")
    res["wal_bytes_after_load"] = os.path.getsize(wal) if os.path.exists(wal) else 0
    res["metadata_db_bytes"] = os.path.getsize(os.path.join(work, "data", "metadata.sqlite3"))
    res["final_rss_kib"] = sampler.rss()
    server.terminate()
    server.wait(timeout=60)
    shutil.rmtree(work, ignore_errors=True)
    print(json.dumps(res, indent=2))


if __name__ == "__main__":
    try:
        main()
    finally:
        subprocess.run(["pkill", "-f", "storlite-bench-"], capture_output=True)
