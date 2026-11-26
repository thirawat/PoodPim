# Serverless backend (AWS Lambda, ap-southeast-7 / Bangkok)

The desktop app can send audio to this instead of running the model locally
(menu → **Serverless (AWS Lambda)**).

## Set it up in your own AWS account

For yourself or your team, one command. It needs the AWS CLI (logged in), Docker with
buildx, curl, unzip and python3:

```sh
./deploy/lambda/setup.sh                                    # name poodpim-asr, Bangkok
NAME=myteam-asr REGION=ap-southeast-1 ./deploy/lambda/setup.sh
```

It downloads the int8 model from Hugging Face (and the int8 decoder from PoodPim's
[model-decoder-int8-1](https://github.com/thirawat/PoodPim/releases/tag/model-decoder-int8-1)
release, checked against its SHA-256) into a private S3 bucket, builds and pushes
the image, creates the IAM role, Lambda function, logs and HTTP API, wakes it once, and
prints the **API URL** and **API key**. Put both in PoodPim (menu → Server URL & API key…)
and share them only with your team. It is safe to re-run: existing pieces are reused and
the key is kept. At most 3 requests run at once (`MAX_CONCURRENCY`, a cap on the bill if
the key leaks).

```sh
NAME=myteam-asr REGION=ap-southeast-1 ./deploy/lambda/deploy.sh     # update the code later
# (also puts the int8 decoder next to the model of a deployment made before it existed)
NAME=myteam-asr REGION=ap-southeast-1 ./deploy/lambda/teardown.sh   # remove everything
```

```
app ──HTTPS──▶ API Gateway HTTP API ──▶ Lambda <name> (arm64, 4 GB, container image)
                (throttled 5 rps, burst 10)        │ cold start: s5cmd pulls the int8 model
                                                   ▼ from S3 into /tmp (~11 s), loads it (~2 s)
                                             S3 <name>-models-<account>-<region>/openthai-asr-int8/
```

Lambda Function URLs are not available in ap-southeast-7 yet, hence API Gateway
(HTTP API, ~$1 per million requests; its integration timeout is 30 s).

## Endpoints

All need header `x-api-key` (the Lambda's `POODPIM_API_KEY` env var; the app keeps the
same key in its config file, mode 600).

| | |
|---|---|
| `POST /warm` | load the model if this instance is cold → `{"load_ms": n}` |
| `POST /transcribe?mixed=1` | body: WAV ≤ 120 s → `{"text", "audio_s", "ms"}` (413 if longer) |

The app sends ≤ 25 s segments cut at pauses (see [docs/how-it-works.md](../../docs/how-it-works.md#long-dictation)), so requests stay far
below Lambda's 6 MB payload limit and API Gateway's 30 s timeout.

The app calls `/warm` when the talk key goes down (so a cold start overlaps with
speaking) and waits for it before sending audio: one Lambda instance serves one
request at a time, so sending in parallel would start a second cold instance.

## Measured (2026-11-15, from Bangkok)

| | |
|---|---|
| cold start (`/warm`) | 13.6 s (S3 fetch 11.3 s, load + warm-up 1.7 s) |
| 6 s sentence, warm | ~0.9 s round trip (Lambda 0.63 s) |
| 32 s recording, warm | ~3.8 s |

With the int8 decoder (2026-11-21, 1.0.5, the app's vocabulary on, 10 test clips of 5-10 s):
each clip took 0.74-1.43 s round trip instead of 1.27-2.37 s with the fp32 decoder (12.1 s
instead of 20.8 s for all ten). Cold start: 16.5 s (one try; 13.9 s just before with fp32).
| memory used | ~2.1 GB of 4 GB |

Why int8 here: the fp32 model (2.5 GB) took 32 s to fetch from S3 (Lambda's network
tops out around 80 MB/s) and 37 s+ when baked into the image, past API Gateway's 30 s
limit. int8 gave the same text as fp32 on all test clips except one dropped tone mark
in a 32 s recording ("ส่วน" → "สวน"). The desktop app uses the same int8 files, including
`decoder_joint.int8.onnx`, which the function prefers to `decoder_joint.onnx` when it is in
the bucket (and falls back from if it can't load or run it).

## Resources made by setup.sh (all tagged `Project=<name>`)

| Resource | Name |
|---|---|
| Lambda | `<name>` (arm64, 4096 MB, timeout 120 s, /tmp 2048 MB), env `POODPIM_API_KEY`, `POODPIM_MODEL_S3` |
| ECR repo | `<name>` (lifecycle: keep last 2 images) |
| S3 bucket | `<name>-models-<account>-<region>`, prefix `openthai-asr-int8/` (private) |
| IAM role | `<name>-lambda-role` (basic logs + read that S3 prefix) |
| API Gateway | HTTP API `<name>`, `$default` route/stage, throttled 5 rps, burst 10 |
| Logs | `/aws/lambda/<name>`, 14-day retention |

## Rotate the API key

Generate a new key, put it in the function's `POODPIM_API_KEY` with
`aws lambda update-function-configuration … --environment …` (keep `POODPIM_MODEL_S3`),
and in the app (menu → Server URL & API key…, or `echo NEWKEY | poodpim server <url>`).

## The original deployment (thocr-asr)

The app was first called thocr, and the first deployment was built by hand before
setup.sh existed. Its resources keep that name: renaming them means recreating them,
which would change the server URL that every installed app points to. Its function uses
`THOCR_API_KEY` / `THOCR_MODEL_S3` (the code reads `POODPIM_*` first and falls back to
`THOCR_*`), its bucket is `thocr-models-<account>-ap-southeast-7` and its role is
`thocr-lambda-role`, so teardown.sh does not fit it. Update its code with
`NAME=thocr-asr ./deploy/lambda/deploy.sh`; remove it by hand:

```sh
R=ap-southeast-7
API=$(aws apigatewayv2 get-apis --region $R --query "Items[?Name=='thocr-asr'].ApiId" --output text)
aws apigatewayv2 delete-api --region $R --api-id $API
aws lambda delete-function --region $R --function-name thocr-asr
aws ecr delete-repository --region $R --repository-name thocr-asr --force
B=thocr-models-$(aws sts get-caller-identity --query Account --output text)-$R
aws s3 rb s3://$B --force --region $R
aws iam delete-role-policy --role-name thocr-lambda-role --policy-name thocr-model-read
aws iam detach-role-policy --role-name thocr-lambda-role --policy-arn arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole
aws iam delete-role --role-name thocr-lambda-role
aws logs delete-log-group --region $R --log-group-name /aws/lambda/thocr-asr
```

## Local test (no AWS)

```sh
docker buildx build --platform linux/arm64 --provenance=false --load -f deploy/lambda/Dockerfile -t thocr-asr:dev .
docker run --rm -p 9000:8080 -e POODPIM_API_KEY=test -e POODPIM_MODEL_DIR=/model \
  -v "$HOME/Library/Application Support/poodpim/openthai-asr:/model:ro" thocr-asr:dev
python3 deploy/lambda/invoke_local.py test warm
python3 deploy/lambda/invoke_local.py test transcribe tests/audio/th1.wav
```
