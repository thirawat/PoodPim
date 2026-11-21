#!/bin/sh
# Sets up the PoodPim serverless backend in your own AWS account, end to end:
#   S3 bucket with the int8 model (downloaded from Hugging Face), ECR repo + image,
#   IAM role, Lambda function (arm64, 4 GB), CloudWatch logs, and an API Gateway HTTP API.
# At the end it prints the API URL and API key to put in the app.
#
#   ./deploy/lambda/setup.sh
#   REGION=ap-southeast-1 NAME=myteam-asr ./deploy/lambda/setup.sh
#
# Safe to re-run: existing pieces are reused, the image and code are updated, and the
# API key is kept. Needs: aws CLI (logged in), docker (with buildx), curl, unzip, python3.
# Remove everything again with ./deploy/lambda/teardown.sh (same NAME / REGION).
set -eu
cd "$(dirname "$0")/../.."

REGION=${REGION:-ap-southeast-7}
NAME=${NAME:-poodpim-asr}
MEMORY=${MEMORY:-4096}
# At most this many requests run at once (a cap on the bill if the key leaks). 0 = no cap.
MAX_CONCURRENCY=${MAX_CONCURRENCY:-3}

for tool in aws docker curl unzip python3; do
  command -v "$tool" >/dev/null || { echo "missing: $tool" >&2; exit 1; }
done

ACCT=$(aws sts get-caller-identity --query Account --output text)
BUCKET=${BUCKET:-$NAME-models-$ACCT-$REGION}
ROLE=$NAME-lambda-role
REG=$ACCT.dkr.ecr.$REGION.amazonaws.com
PREFIX=openthai-asr-int8
HF=https://huggingface.co/iapp/OpenThai2.0-ASR-Streaming-ONNX/resolve/main
TAGS="Key=Project,Value=$NAME"
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
IMAGE_TAG=v$VERSION-$(date +%Y%m%d%H%M%S)
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
step() { printf '\n== %s\n' "$*"; }

step "S3 bucket $BUCKET"
if ! aws s3api head-bucket --bucket "$BUCKET" --region "$REGION" 2>/dev/null; then
  if [ "$REGION" = us-east-1 ]; then
    aws s3api create-bucket --bucket "$BUCKET" --region "$REGION" >/dev/null
  else
    aws s3api create-bucket --bucket "$BUCKET" --region "$REGION" \
      --create-bucket-configuration LocationConstraint="$REGION" >/dev/null
  fi
fi
aws s3api put-public-access-block --bucket "$BUCKET" --region "$REGION" --public-access-block-configuration \
  BlockPublicAcls=true,IgnorePublicAcls=true,BlockPublicPolicy=true,RestrictPublicBuckets=true
aws s3api put-bucket-tagging --bucket "$BUCKET" --region "$REGION" --tagging "TagSet=[{Key=Project,Value=$NAME}]"

step "Model (OpenThai2.0-ASR-Streaming, int8) in s3://$BUCKET/$PREFIX/"
if aws s3api head-object --bucket "$BUCKET" --region "$REGION" --key "$PREFIX/encoder.int8.onnx.data" >/dev/null 2>&1; then
  echo "already there"
else
  M=target/hf-model/int8
  mkdir -p "$M"
  [ -f "$M/encoder.int8.onnx.data" ] || curl -fL --progress-bar -o "$M/encoder.int8.onnx.data" "$HF/encoder.int8.onnx.data"
  [ -f "$M/decoder_joint.onnx" ] || {
    curl -fL --progress-bar -o "$TMP/int8.zip" "$HF/openthai2.0-asr-streaming-int8.asr.zip"
    unzip -o -q "$TMP/int8.zip" -d "$M"
  }
  [ -f "$M/LICENSE" ] || curl -fsSL -o "$M/LICENSE" "$HF/LICENSE"
  for f in encoder.int8.onnx encoder.int8.onnx.data decoder_joint.onnx tokens.txt LICENSE; do
    aws s3 cp --region "$REGION" --only-show-errors "$M/$f" "s3://$BUCKET/$PREFIX/$f"
  done
  echo "uploaded"
fi
./deploy/lambda/add-int8-decoder.sh "s3://$BUCKET/$PREFIX/" "$REGION"

step "ECR repository $NAME"
aws ecr describe-repositories --region "$REGION" --repository-names "$NAME" >/dev/null 2>&1 ||
  aws ecr create-repository --region "$REGION" --repository-name "$NAME" --tags "$TAGS" >/dev/null
aws ecr put-lifecycle-policy --region "$REGION" --repository-name "$NAME" --lifecycle-policy-text \
  '{"rules":[{"rulePriority":1,"description":"keep last 2 images","selection":{"tagStatus":"any","countType":"imageCountMoreThan","countNumber":2},"action":{"type":"expire"}}]}' >/dev/null

step "Image $NAME:$IMAGE_TAG (linux/arm64)"
docker buildx build --platform linux/arm64 --provenance=false --load -f deploy/lambda/Dockerfile -t "$NAME:build" .
# Fetch first: in a pipe, sh would hide an aws failure behind a vague docker error.
ECR_PASS=$(aws ecr get-login-password --region "$REGION")
printf '%s' "$ECR_PASS" | docker login --username AWS --password-stdin "$REG" >/dev/null
docker tag "$NAME:build" "$REG/$NAME:$IMAGE_TAG"
docker push --quiet "$REG/$NAME:$IMAGE_TAG" >/dev/null
IMAGE_URI=$REG/$NAME:$IMAGE_TAG

step "IAM role $ROLE"
if ! aws iam get-role --role-name "$ROLE" >/dev/null 2>&1; then
  aws iam create-role --role-name "$ROLE" --tags "$TAGS" --assume-role-policy-document \
    '{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Principal":{"Service":"lambda.amazonaws.com"},"Action":"sts:AssumeRole"}]}' >/dev/null
  aws iam attach-role-policy --role-name "$ROLE" --policy-arn arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole
  NEW_ROLE=1
fi
aws iam put-role-policy --role-name "$ROLE" --policy-name model-read --policy-document \
  "{\"Version\":\"2012-10-17\",\"Statement\":[{\"Effect\":\"Allow\",\"Action\":\"s3:GetObject\",\"Resource\":\"arn:aws:s3:::$BUCKET/$PREFIX/*\"},{\"Effect\":\"Allow\",\"Action\":\"s3:ListBucket\",\"Resource\":\"arn:aws:s3:::$BUCKET\",\"Condition\":{\"StringLike\":{\"s3:prefix\":[\"$PREFIX/*\"]}}}]}"
# A new role takes a few seconds before Lambda may assume it.
[ "${NEW_ROLE:-0}" = 1 ] && sleep 12

step "Logs /aws/lambda/$NAME (14 days)"
aws logs create-log-group --region "$REGION" --log-group-name "/aws/lambda/$NAME" --tags "Project=$NAME" 2>/dev/null || true
aws logs put-retention-policy --region "$REGION" --log-group-name "/aws/lambda/$NAME" --retention-in-days 14

step "Lambda function $NAME"
umask 077
if aws lambda get-function --region "$REGION" --function-name "$NAME" >/dev/null 2>&1; then
  aws lambda update-function-code --region "$REGION" --function-name "$NAME" --image-uri "$IMAGE_URI" >/dev/null
  aws lambda wait function-updated-v2 --region "$REGION" --function-name "$NAME"
  # Keep the existing key (also from functions set up before the rename, as THOCR_API_KEY).
  aws lambda get-function-configuration --region "$REGION" --function-name "$NAME" \
    --query 'Environment.Variables' --output json > "$TMP/env.json"
  KEY=$(python3 -c "import json,sys; v=json.load(open(sys.argv[1])) or {}; print(v.get('POODPIM_API_KEY') or v.get('THOCR_API_KEY') or '')" "$TMP/env.json")
  echo "code updated"
else
  KEY=$(python3 -c "import secrets; print(secrets.token_urlsafe(32))")
  python3 - "$TMP/fn.json" "$NAME" "$IMAGE_URI" "arn:aws:iam::$ACCT:role/$ROLE" "$MEMORY" "$KEY" "s3://$BUCKET/$PREFIX/" <<'PY'
import json, sys
out, name, image, role, memory, key, model = sys.argv[1:]
json.dump({
    "FunctionName": name, "PackageType": "Image", "Code": {"ImageUri": image}, "Role": role,
    "Architectures": ["arm64"], "MemorySize": int(memory), "Timeout": 120,
    "EphemeralStorage": {"Size": 2048},
    "Description": "PoodPim Thai dictation: OpenThai2.0-ASR-Streaming (ONNX, int8)",
    "Environment": {"Variables": {"POODPIM_API_KEY": key, "POODPIM_MODEL_S3": model}},
    "LoggingConfig": {"LogFormat": "Text", "LogGroup": f"/aws/lambda/{name}"},
    "Tags": {"Project": name},
}, open(out, "w"))
PY
  aws lambda create-function --region "$REGION" --cli-input-json "file://$TMP/fn.json" >/dev/null
  rm -f "$TMP/fn.json"
  aws lambda wait function-active-v2 --region "$REGION" --function-name "$NAME"
  echo "created"
fi
if [ "$MAX_CONCURRENCY" -gt 0 ]; then
  aws lambda put-function-concurrency --region "$REGION" --function-name "$NAME" \
    --reserved-concurrent-executions "$MAX_CONCURRENCY" >/dev/null 2>&1 &&
    echo "at most $MAX_CONCURRENCY requests at once" ||
    echo "note: could not reserve concurrency (new accounts may have too low a limit); continuing without a cap"
fi

step "API Gateway HTTP API $NAME"
FN_ARN=arn:aws:lambda:$REGION:$ACCT:function:$NAME
API_ID=$(aws apigatewayv2 get-apis --region "$REGION" --query "Items[?Name=='$NAME'].ApiId | [0]" --output text)
if [ -z "$API_ID" ] || [ "$API_ID" = None ]; then
  API_ID=$(aws apigatewayv2 create-api --region "$REGION" --name "$NAME" --protocol-type HTTP \
    --target "$FN_ARN" --tags "Project=$NAME" --query ApiId --output text)
fi
aws lambda add-permission --region "$REGION" --function-name "$NAME" --statement-id apigw-invoke \
  --action lambda:InvokeFunction --principal apigateway.amazonaws.com \
  --source-arn "arn:aws:execute-api:$REGION:$ACCT:$API_ID/*" >/dev/null 2>&1 || true
aws apigatewayv2 update-stage --region "$REGION" --api-id "$API_ID" --stage-name '$default' \
  --default-route-settings ThrottlingBurstLimit=10,ThrottlingRateLimit=5 >/dev/null
URL=https://$API_ID.execute-api.$REGION.amazonaws.com

step "Waking it up once (first start downloads the model, ~15 s)"
# A brand-new API answers "Not Found" for a few seconds until its stage is live.
for _ in 1 2 3 4 5 6; do
  REPLY=$(curl -s -m 60 -X POST -H "x-api-key: $KEY" "$URL/warm" || true)
  case $REPLY in *load_ms*) break ;; esac
  sleep 5
done
echo "$REPLY"

cat <<EOF

Done. Put these in PoodPim (menu → Server URL & API key…), and share them only with your team:

  API URL: $URL
  API key: $KEY

Update the code later:  NAME=$NAME REGION=$REGION ./deploy/lambda/deploy.sh
Remove everything:      NAME=$NAME REGION=$REGION ./deploy/lambda/teardown.sh
EOF
