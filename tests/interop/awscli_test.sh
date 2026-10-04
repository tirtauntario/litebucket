#!/usr/bin/env bash
# SDK-01 (AWS CLI v2): default-checksum ordinary and forced-multipart flows
# against a local storlite only. Run via scripts/interop.sh.
set -euo pipefail
case "$STORLITE_ENDPOINT" in http://127.0.0.1:*|https://127.0.0.1:*) ;; *) echo "refusing non-local endpoint"; exit 1 ;; esac

W="$INTEROP_WORK/awscli"; mkdir -p "$W"
export AWS_CONFIG_FILE="$W/config" AWS_SHARED_CREDENTIALS_FILE="$W/credentials"
cat > "$AWS_CONFIG_FILE" <<CFG
[default]
region = $STORLITE_REGION
s3 =
  addressing_style = path
  multipart_threshold = 5MB
  multipart_chunksize = 5MB
CFG
cat > "$AWS_SHARED_CREDENTIALS_FILE" <<CRED
[default]
aws_access_key_id = $STORLITE_KEY_ID
aws_secret_access_key = $STORLITE_SECRET
CRED
CA_ARGS=(); [ -n "${STORLITE_CA_BUNDLE:-}" ] && CA_ARGS=(--ca-bundle "$STORLITE_CA_BUNDLE")
CURL_CA=(); [ -n "${STORLITE_CA_BUNDLE:-}" ] && CURL_CA=(--cacert "$STORLITE_CA_BUNDLE")
aws() { command aws --endpoint-url "$STORLITE_ENDPOINT" ${CA_ARGS[@]+"${CA_ARGS[@]}"} "$@"; }
pass=0; fail=0
check() { if eval "$2"; then echo "ok   $1"; pass=$((pass+1)); else echo "FAIL $1"; fail=$((fail+1)); fi; }

command aws --version
B="cli-$(openssl rand -hex 4)"
aws s3 mb "s3://$B" >/dev/null
head -c 300000 /dev/urandom > "$W/small.bin"
head -c 23068672 /dev/urandom > "$W/big.bin"   # 22 MiB -> forced multipart
mkdir -p "$W/tree/sub"; echo one > "$W/tree/1.txt"; echo two > "$W/tree/sub/2.txt"

aws s3 cp "$W/small.bin" "s3://$B/small.bin" >/dev/null
aws s3 cp "s3://$B/small.bin" "$W/small.down" >/dev/null
check "small upload/download" "cmp -s '$W/small.bin' '$W/small.down'"
aws s3 cp "$W/big.bin" "s3://$B/big.bin" >/dev/null
etag=$(aws s3api head-object --bucket "$B" --key big.bin --query ETag --output text)
check "forced multipart upload (ETag $etag)" "[[ '$etag' == *-5\\\" ]]"
aws s3 cp "s3://$B/big.bin" "$W/big.down" >/dev/null
check "multipart download integrity" "cmp -s '$W/big.bin' '$W/big.down'"
sum=$(aws s3api head-object --bucket "$B" --key small.bin --checksum-mode ENABLED --query 'join(`,`, [ChecksumCRC32 || ``, ChecksumCRC64NVME || ``])' --output text)
check "default checksum stored ($sum)" "[ -n '${sum//,/}' ]"
aws s3 cp "$W/small.bin" "s3://$B/small.bin" --metadata owner=cli --content-type text/x-cli >/dev/null
meta=$(aws s3api head-object --bucket "$B" --key small.bin --query 'join(`/`, [Metadata.owner, ContentType])' --output text)
check "overwrite with metadata ($meta)" "[ '$meta' = 'cli/text/x-cli' ]"
aws s3 sync "$W/tree" "s3://$B/tree" >/dev/null
check "sync + recursive ls" "[ \$(aws s3 ls --recursive s3://$B/tree/ | wc -l) -eq 2 ]"
n=$(aws s3api list-objects-v2 --bucket "$B" --page-size 1 --query 'Contents[].Key' --output text | wc -w | tr -d ' ')
check "paginated list-objects-v2 ($n keys)" "[ '$n' = '4' ]"
url=$(aws s3 presign "s3://$B/small.bin" --expires-in 300)
curl -sf ${CURL_CA[@]+"${CURL_CA[@]}"} "$url" -o "$W/presigned.down"
check "presigned GET" "cmp -s '$W/small.bin' '$W/presigned.down'"
aws s3api put-object --bucket "$B" --key c32 --body "$W/small.bin" --checksum-algorithm CRC32C >/dev/null
check "explicit CRC32C" "aws s3api head-object --bucket $B --key c32 --checksum-mode ENABLED --query ChecksumCRC32C --output text | grep -q ="
aws s3 cp "s3://$B/small.bin" "s3://$B/copy.bin" >/dev/null
check "server-side copy" "aws s3api head-object --bucket $B --key copy.bin >/dev/null"
aws s3 rm --recursive "s3://$B" >/dev/null
check "recursive delete (DeleteObjects)" "[ -z \"\$(aws s3 ls s3://$B)\" ]"
aws s3 rb "s3://$B" >/dev/null
check "remove bucket" "! aws s3api head-bucket --bucket $B 2>/dev/null"
echo "awscli: $pass passed, $fail failed"
[ "$fail" -eq 0 ]
