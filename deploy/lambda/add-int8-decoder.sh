#!/bin/sh
# Puts decoder_joint.int8.onnx (from PoodPim's release model-decoder-int8-1, made by
# tools/quantize-decoder.py) next to the model in S3, which the Lambda downloads on a cold
# start; the function then uses it instead of decoder_joint.onnx (2.7x less decoder time, as
# accurate on tools/asr-eval). Does nothing if it is already there. setup.sh and deploy.sh run it.
#   ./deploy/lambda/add-int8-decoder.sh s3://bucket/openthai-asr-int8/ [region]
set -eu
cd "$(dirname "$0")/../.."

S3=${1%/}
REGION=${2:-ap-southeast-7}
F=decoder_joint.int8.onnx
URL=https://github.com/thirawat/PoodPim/releases/download/model-decoder-int8-1/$F
SHA256=f5f5a06684cb64e94487d9b1d411ec0829c7a6fa5c1af74c87c0e1a9577f6c7e

if aws s3 ls --region "$REGION" "$S3/$F" >/dev/null 2>&1; then
  echo "$F already in $S3/"
  exit 0
fi
M=target/hf-model/int8
mkdir -p "$M"
sha() { python3 -c 'import hashlib,sys; print(hashlib.sha256(open(sys.argv[1],"rb").read()).hexdigest())' "$1"; }
if [ ! -f "$M/$F" ] || [ "$(sha "$M/$F")" != "$SHA256" ]; then
  curl -fL --progress-bar -o "$M/$F" "$URL"
fi
[ "$(sha "$M/$F")" = "$SHA256" ] || { echo "$F: SHA-256 does not match the release" >&2; exit 1; }
aws s3 cp --region "$REGION" --only-show-errors "$M/$F" "$S3/$F"
echo "uploaded $F to $S3/"
