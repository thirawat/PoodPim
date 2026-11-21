# syntax=docker/dockerfile:1
# Lambda container image for poodpim-lambda (arm64 / Graviton).
# The model is not in the image: the function downloads it from S3 ($POODPIM_MODEL_S3) on cold start.
# Build from the repo root:
#   docker buildx build --platform linux/arm64 --provenance=false -f deploy/lambda/Dockerfile -t thocr-asr .

# Build on Amazon Linux 2023 so the binary's glibc matches the Lambda runtime.
FROM public.ecr.aws/amazonlinux/amazonlinux:2023 AS build
RUN dnf install -y gcc gcc-c++ make tar gzip xz openssl-devel pkgconf-pkg-config && dnf clean all
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain 1.96.1
ENV PATH=/root/.cargo/bin:$PATH
WORKDIR /src
COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
# Default word lists the code embeds (include_str!).
COPY assets/vocabulary.txt assets/corrections.txt ./assets/
RUN --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --no-default-features --features lambda --bin poodpim-lambda \
 && cp target/release/poodpim-lambda /bootstrap

FROM public.ecr.aws/lambda/provided:al2023-arm64
# ONNX Runtime from Microsoft's NuGet package (runtimes/linux-arm64/native). The GitHub repo
# microsoft/onnxruntime and its release downloads went 404 on 2026-11-18. The package is
# pinned by SHA-256; its libonnxruntime.so is byte-identical to the GitHub release tarball's.
ARG ORT_VERSION=1.32.0
ARG ORT_NUPKG_SHA256=a9150bc2992cbbb6722070da9910e1ffa989a8bfc67bdd5bb8e745ca755641db
RUN dnf install -y libstdc++ tar gzip unzip && dnf clean all \
 && mkdir -p /opt/ort/lib && cd /tmp \
 && curl -sSfL -o ort.nupkg https://api.nuget.org/v3-flatcontainer/microsoft.ml.onnxruntime/${ORT_VERSION}/microsoft.ml.onnxruntime.${ORT_VERSION}.nupkg \
 && echo "${ORT_NUPKG_SHA256}  ort.nupkg" | sha256sum -c - \
 && unzip -j -q ort.nupkg 'runtimes/linux-arm64/native/*' -d /opt/ort/lib \
 && rm ort.nupkg
ENV ORT_DYLIB_PATH=/opt/ort/lib/libonnxruntime.so
ARG S5CMD_VERSION=2.3.0
RUN mkdir -p /opt/bin && cd /tmp \
 && F=s5cmd_${S5CMD_VERSION}_Linux-arm64.tar.gz \
 && curl -sSfLO https://github.com/peak/s5cmd/releases/download/v${S5CMD_VERSION}/$F \
 && curl -sSfL https://github.com/peak/s5cmd/releases/download/v${S5CMD_VERSION}/s5cmd_checksums.txt | grep " $F$" | sha256sum -c - \
 && tar -xzf $F -C /opt/bin s5cmd && rm $F
COPY --from=build /bootstrap ${LAMBDA_RUNTIME_DIR}/bootstrap
CMD ["poodpim.handler"]
