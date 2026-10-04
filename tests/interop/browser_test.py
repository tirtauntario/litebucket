"""SDK-04: real browser (headless Chrome) CORS preflight and signed upload.

A page served from the allowed origin PUTs to a presigned URL and reads the
result; a page from a forbidden origin must be blocked by the preflight; an
unsigned PUT from the allowed origin must still fail authentication.
Run via scripts/interop.sh."""

import http.server
import json
import os
import re
import secrets
import socket
import subprocess
import sys
import tempfile
import threading

import boto3
from botocore.config import Config

ENDPOINT = os.environ["STORLITE_ENDPOINT"]
if not (ENDPOINT.startswith("http://127.0.0.1:") or ENDPOINT.startswith("https://127.0.0.1:")):
    raise SystemExit("refusing non-local endpoint")
CA = os.environ.get("STORLITE_CA_BUNDLE")
CHROME = os.environ.get("CHROME", "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")

s3 = boto3.client(
    "s3",
    endpoint_url=ENDPOINT,
    region_name=os.environ["STORLITE_REGION"],
    aws_access_key_id=os.environ["STORLITE_KEY_ID"],
    aws_secret_access_key=os.environ["STORLITE_SECRET"],
    config=Config(signature_version="s3v4", s3={"addressing_style": "path"}),
    verify=CA if CA else None,
)


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


PAGE = """<!doctype html><html><body><pre id="result">pending</pre><script>
// Synchronous XHR completes before --dump-dom captures the document.
function req(method, url, body) {
  const x = new XMLHttpRequest();
  try {
    x.open(method, url, false);
    if (method === 'PUT') x.setRequestHeader('Content-Type', 'text/plain');
    x.send(body || null);
    return {status: x.status, etag: x.getResponseHeader('ETag'), body: x.responseText};
  } catch (e) { return {status: 'blocked'}; }
}
const out = {};
const p = req('PUT', %(put)s, 'from browser'); out.put = p.status; out.etag = p.etag;
const g = req('GET', %(get)s); out.get = g.status; out.body = g.body;
out.unsigned = req('PUT', %(unsigned)s, 'x').status;
document.getElementById('result').textContent = JSON.stringify(out);
</script></body></html>"""


def serve(directory, port):
    handler = lambda *a, **k: http.server.SimpleHTTPRequestHandler(*a, directory=directory, **k)
    httpd = http.server.ThreadingHTTPServer(("127.0.0.1", port), handler)
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    return httpd


def run_page(url, profile):
    # Headless Chrome may keep running after --dump-dom prints the document,
    # so read until the document is complete and then stop it.
    proc = subprocess.Popen(
        [CHROME, "--headless=new", "--disable-gpu", "--no-first-run", "--no-default-browser-check",
         f"--user-data-dir={profile}", "--dump-dom", url]
        # Throwaway self-signed test certificate in TLS mode only.
        + (["--ignore-certificate-errors"] if CA else []),
        stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True,
    )
    out = ""
    timer = threading.Timer(90, proc.kill)
    timer.start()
    try:
        for line in proc.stdout:
            out += line
            if "</html>" in line:
                break
    finally:
        timer.cancel()
        proc.kill()
        proc.wait()
    m = re.search(r'<pre id="result">(.*?)</pre>', out, re.S)
    if not m or m.group(1) == "pending":
        raise SystemExit(f"page did not finish: {out[-500:]}")
    return json.loads(m.group(1).replace("&quot;", '"').replace("&amp;", "&"))


def main():
    version = subprocess.run([CHROME, "--version"], capture_output=True, text=True).stdout.strip()
    print(version)
    bucket = "browser-" + secrets.token_hex(4)
    s3.create_bucket(Bucket=bucket)
    allowed_port, forbidden_port = free_port(), free_port()
    allowed = f"http://127.0.0.1:{allowed_port}"
    s3.put_bucket_cors(
        Bucket=bucket,
        CORSConfiguration={"CORSRules": [{
            "AllowedOrigins": [allowed], "AllowedMethods": ["PUT", "GET"],
            "AllowedHeaders": ["*"], "ExposeHeaders": ["ETag"], "MaxAgeSeconds": 60,
        }]},
    )
    results = {}
    with tempfile.TemporaryDirectory() as tmp:
        for name, port, key in [("allowed", allowed_port, "ok.txt"), ("forbidden", forbidden_port, "forbidden.txt")]:
            d = os.path.join(tmp, name)
            os.makedirs(d)
            params = {"Bucket": bucket, "Key": key}
            put = s3.generate_presigned_url("put_object", Params=params, ExpiresIn=300)
            get = s3.generate_presigned_url("get_object", Params=params, ExpiresIn=300)
            unsigned = f"{ENDPOINT}/{bucket}/unsigned-{name}.txt"
            with open(os.path.join(d, "page.html"), "w") as f:
                f.write(PAGE % {"put": json.dumps(put), "get": json.dumps(get), "unsigned": json.dumps(unsigned)})
            serve(d, port)
            results[name] = run_page(f"http://127.0.0.1:{port}/page.html", os.path.join(tmp, name + "-profile"))
            print(name, results[name])
    a, f = results["allowed"], results["forbidden"]
    checks = [
        ("allowed origin: preflight + signed PUT", a["put"] == 200),
        ("allowed origin: exposed ETag readable", bool(a.get("etag"))),
        ("allowed origin: presigned GET body", a["get"] == 200 and a.get("body") == "from browser"),
        ("CORS does not bypass authentication", a["unsigned"] == 403),
        ("forbidden origin: PUT blocked", f["put"] == "blocked"),
        ("forbidden origin: GET not readable", f["get"] == "blocked"),
    ]
    try:
        s3.head_object(Bucket=bucket, Key="forbidden.txt")
        stored = True
    except s3.exceptions.ClientError:
        stored = False
    checks.append(("forbidden origin: nothing stored", not stored))
    for k in ["unsigned-allowed.txt", "unsigned-forbidden.txt"]:
        try:
            s3.head_object(Bucket=bucket, Key=k)
            checks.append((f"{k} not stored", False))
        except s3.exceptions.ClientError:
            checks.append((f"{k} not stored", True))
    failed = 0
    for name, ok in checks:
        print(("ok   " if ok else "FAIL ") + name)
        failed += not ok
    print(f"browser: {len(checks) - failed} passed, {failed} failed")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
