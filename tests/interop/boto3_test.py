"""SDK-02 (Boto3): default checksums, metadata, listing, presign, forced
multipart transfers against a local storlite. Run via scripts/interop.sh."""

import hashlib
import os
import secrets
import unittest
import urllib.request

import boto3
import botocore
from boto3.s3.transfer import TransferConfig
from botocore.config import Config

ENDPOINT = os.environ["STORLITE_ENDPOINT"]
if not ENDPOINT.startswith("http://127.0.0.1:"):
    raise SystemExit(f"refusing non-local endpoint {ENDPOINT}")


def client():
    return boto3.client(
        "s3",
        endpoint_url=ENDPOINT,
        region_name=os.environ["STORLITE_REGION"],
        aws_access_key_id=os.environ["STORLITE_KEY_ID"],
        aws_secret_access_key=os.environ["STORLITE_SECRET"],
        # SigV4 is required; botocore's legacy presigner default would use SigV2.
        config=Config(signature_version="s3v4", s3={"addressing_style": "path"}, retries={"max_attempts": 2}),
    )


S3 = client()
MIB = 1024 * 1024


class Boto3Test(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        print(f"boto3 {boto3.__version__}, botocore {botocore.__version__}")
        print(
            "request_checksum_calculation=%s response_checksum_validation=%s"
            % (S3.meta.config.request_checksum_calculation, S3.meta.config.response_checksum_validation)
        )
        cls.bucket = "boto-" + secrets.token_hex(4)
        S3.create_bucket(Bucket=cls.bucket)

    def test_round_trip_metadata_and_checksums(self):
        body = secrets.token_bytes(300_000)
        r = S3.put_object(
            Bucket=self.bucket, Key="a/b.bin", Body=body, ContentType="application/x-test",
            Metadata={"owner": "boto"}, ContentDisposition='attachment; filename="b.bin"',
        )
        self.assertEqual(r["ETag"], '"%s"' % hashlib.md5(body).hexdigest())
        g = S3.get_object(Bucket=self.bucket, Key="a/b.bin", ChecksumMode="ENABLED")
        self.assertEqual(g["Body"].read(), body)
        self.assertEqual(g["ContentType"], "application/x-test")
        self.assertEqual(g["Metadata"], {"owner": "boto"})
        self.assertEqual(g["ContentDisposition"], 'attachment; filename="b.bin"')
        h = S3.head_object(Bucket=self.bucket, Key="a/b.bin", ChecksumMode="ENABLED")
        self.assertEqual(h["ContentLength"], 300_000)
        self.assertTrue(any(k.startswith("Checksum") and k != "ChecksumType" for k in h), h.keys())
        self.assertEqual(S3.get_object(Bucket=self.bucket, Key="a/b.bin", Range="bytes=10-19")["Body"].read(), body[10:20])

    def test_all_checksum_algorithms(self):
        for alg in ["CRC32", "CRC32C", "CRC64NVME", "SHA1", "SHA256"]:
            data = f"payload for {alg}".encode()
            r = S3.put_object(Bucket=self.bucket, Key=f"sum/{alg}", Body=data, ChecksumAlgorithm=alg)
            self.assertIn(f"Checksum{alg}", r)
            g = S3.get_object(Bucket=self.bucket, Key=f"sum/{alg}", ChecksumMode="ENABLED")
            self.assertEqual(g["Body"].read(), data)
            self.assertEqual(g[f"Checksum{alg}"], r[f"Checksum{alg}"])

    def test_paginated_listing(self):
        keys = [f"page/{i:03d}" for i in range(23)] + ["page/sub/x", "page/sub/y"]
        for k in keys:
            S3.put_object(Bucket=self.bucket, Key=k, Body=b"x")
        got = []
        for page in S3.get_paginator("list_objects_v2").paginate(
            Bucket=self.bucket, Prefix="page/", PaginationConfig={"PageSize": 5}
        ):
            got += [o["Key"] for o in page.get("Contents", [])]
        self.assertEqual(sorted(got), sorted(keys))
        d = S3.list_objects_v2(Bucket=self.bucket, Prefix="page/", Delimiter="/")
        self.assertEqual([p["Prefix"] for p in d["CommonPrefixes"]], ["page/sub/"])
        self.assertEqual(d["KeyCount"], 24)

    def test_forced_multipart_transfer(self):
        path = os.path.join(os.environ["INTEROP_WORK"], "boto-big-" + secrets.token_hex(4))
        with open(path, "wb") as f:
            f.write(secrets.token_bytes(23 * MIB))
        cfg = TransferConfig(multipart_threshold=5 * MIB, multipart_chunksize=5 * MIB, max_concurrency=4)
        S3.upload_file(path, self.bucket, "big.bin", Config=cfg)
        h = S3.head_object(Bucket=self.bucket, Key="big.bin", ChecksumMode="ENABLED")
        self.assertTrue(h["ETag"].endswith('-5"'), h["ETag"])
        S3.download_file(self.bucket, "big.bin", path + ".down", Config=cfg)
        def sha(p):
            with open(p, "rb") as f:
                return hashlib.sha256(f.read()).hexdigest()

        self.assertEqual(sha(path), sha(path + ".down"))

    def test_low_level_multipart_with_composite_checksum(self):
        mp = S3.create_multipart_upload(Bucket=self.bucket, Key="mp", ChecksumAlgorithm="SHA256")
        parts = []
        for n, data in [(1, secrets.token_bytes(5 * MIB)), (2, b"tail")]:
            r = S3.upload_part(Bucket=self.bucket, Key="mp", UploadId=mp["UploadId"], PartNumber=n, Body=data, ChecksumAlgorithm="SHA256")
            parts.append({"PartNumber": n, "ETag": r["ETag"], "ChecksumSHA256": r["ChecksumSHA256"]})
        listed = S3.list_parts(Bucket=self.bucket, Key="mp", UploadId=mp["UploadId"])
        self.assertEqual([p["PartNumber"] for p in listed["Parts"]], [1, 2])
        ups = S3.list_multipart_uploads(Bucket=self.bucket)
        self.assertIn(mp["UploadId"], [u["UploadId"] for u in ups.get("Uploads", [])])
        done = S3.complete_multipart_upload(Bucket=self.bucket, Key="mp", UploadId=mp["UploadId"], MultipartUpload={"Parts": parts})
        self.assertTrue(done["ChecksumSHA256"].endswith("-2"))
        self.assertEqual(S3.head_object(Bucket=self.bucket, Key="mp")["ContentLength"], 5 * MIB + 4)

    def test_copy_delete_conditional_presign(self):
        S3.put_object(Bucket=self.bucket, Key="src", Body=b"copy", Metadata={"k": "v"})
        S3.copy_object(Bucket=self.bucket, Key="dst", CopySource={"Bucket": self.bucket, "Key": "src"})
        self.assertEqual(S3.get_object(Bucket=self.bucket, Key="dst")["Metadata"], {"k": "v"})
        S3.copy_object(
            Bucket=self.bucket, Key="dst2", CopySource=f"{self.bucket}/src", MetadataDirective="REPLACE",
            Metadata={"n": "1"}, ContentType="text/plain",
        )
        self.assertEqual(S3.head_object(Bucket=self.bucket, Key="dst2")["Metadata"], {"n": "1"})
        r = S3.delete_objects(Bucket=self.bucket, Delete={"Objects": [{"Key": "src"}, {"Key": "dst"}, {"Key": "nope"}]})
        self.assertEqual(sorted(d["Key"] for d in r["Deleted"]), ["dst", "nope", "src"])
        with self.assertRaises(botocore.exceptions.ClientError) as e:
            S3.head_object(Bucket=self.bucket, Key="src")
        self.assertEqual(e.exception.response["Error"]["Code"], "404")
        S3.put_object(Bucket=self.bucket, Key="once", Body=b"1", IfNoneMatch="*")
        with self.assertRaises(botocore.exceptions.ClientError) as e:
            S3.put_object(Bucket=self.bucket, Key="once", Body=b"2", IfNoneMatch="*")
        self.assertEqual(e.exception.response["Error"]["Code"], "PreconditionFailed")
        put_url = S3.generate_presigned_url("put_object", Params={"Bucket": self.bucket, "Key": "pre"}, ExpiresIn=300)
        urllib.request.urlopen(urllib.request.Request(put_url, data=b"presigned", method="PUT")).read()
        get_url = S3.generate_presigned_url("get_object", Params={"Bucket": self.bucket, "Key": "pre"}, ExpiresIn=300)
        self.assertEqual(urllib.request.urlopen(get_url).read(), b"presigned")

    def test_bucket_operations_and_errors(self):
        self.assertIn(self.bucket, [b["Name"] for b in S3.list_buckets()["Buckets"]])
        self.assertIn(S3.get_bucket_location(Bucket=self.bucket)["LocationConstraint"], (None, ""))
        S3.head_bucket(Bucket=self.bucket)
        with self.assertRaises(botocore.exceptions.ClientError) as e:
            S3.get_object(Bucket=self.bucket, Key="missing")
        self.assertEqual(e.exception.response["Error"]["Code"], "NoSuchKey")
        with self.assertRaises(botocore.exceptions.ClientError) as e:
            S3.put_object(Bucket=self.bucket, Key="enc", Body=b"x", ServerSideEncryption="AES256")
        self.assertEqual(e.exception.response["Error"]["Code"], "NotImplemented")
        tmp = "boto-tmp-" + secrets.token_hex(3)
        S3.create_bucket(Bucket=tmp)
        S3.put_bucket_cors(
            Bucket=tmp,
            CORSConfiguration={"CORSRules": [{"AllowedOrigins": ["https://a.example"], "AllowedMethods": ["GET"]}]},
        )
        self.assertEqual(S3.get_bucket_cors(Bucket=tmp)["CORSRules"][0]["AllowedOrigins"], ["https://a.example"])
        S3.delete_bucket_cors(Bucket=tmp)
        S3.delete_bucket(Bucket=tmp)


if __name__ == "__main__":
    unittest.main(verbosity=2)
