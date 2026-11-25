#!/bin/sh
# Writes THIRD-PARTY-NOTICES.txt: the licences of everything built into the desktop app that
# isn't PoodPim's own code, for macOS and Windows.
#   1. Native libraries the crates link in, which cargo-about can't see: libopus (its text is
#      in the opusic-sys crate), ONNX Runtime with its own notices, and on Windows the
#      WebView2 loader (the last two kept in licenses/, see the SOURCE.txt there).
#   2. Every Rust crate in the app, by cargo-about (about.toml, about.hbs): build tools and
#      test-only crates are left out, as they aren't in the app.
# Needs cargo-about: `cargo install --locked cargo-about --features cli`. Run it again and
# commit the result when Cargo.lock changes; replace licenses/onnxruntime-* when ort moves to
# another ONNX Runtime version. The packages ship the file (bundle-macos.sh,
# package-macos.sh, package-windows.sh). The model and DirectML.dll come with their own.
set -eu
cd "$(dirname "$0")/.."

command -v cargo-about >/dev/null || { echo "needs cargo-about: cargo install --locked cargo-about --features cli" >&2; exit 1; }

# name version of a package in Cargo.lock
version() { awk -v n="$1" '$0 == "name = \"" n "\"" { getline; gsub(/version = |"/, ""); print; exit }' Cargo.lock; }
REG=$(cargo metadata --format-version 1 --locked >/dev/null && ls -d "${CARGO_HOME:-$HOME/.cargo}"/registry/src/index.crates.io-*/ | head -1)
OPUSIC=$REG/opusic-sys-$(version opusic-sys)
OPUS_VERSION=$(sed -n 's/^PACKAGE_VERSION="\(.*\)"/\1/p' "$OPUSIC/opus/package_version")
ORT_DIR=$(ls -d licenses/onnxruntime-* | tail -1)
CRATES=$(mktemp)
trap 'rm -f "$CRATES"' EXIT
cargo about generate --locked about.hbs -o "$CRATES"

rule() { echo "================================================================================"; }
{
  cat <<EOF
PoodPim — third-party notices

PoodPim's own code is under the MIT licence (LICENSE). The app is built with the software
below, whose licences ask for their text to go with copies of the app. Part 1 is native
libraries linked into the app; part 2 is the Rust crates it is made of. The source code of
each crate in part 2 is available at https://crates.io/crates/<name>/<version> (it includes
Symphonia, under the Mozilla Public License 2.0, used unmodified). On Windows, DirectML.dll
comes with its own licence (LICENSE-DirectML.txt). The speech model is downloaded by the app
separately, under its own licence (OpenMDW-1.1, in the model folder).

EOF
  rule
  echo "PART 1. NATIVE LIBRARIES"
  rule
  cat <<EOF

1a. Opus audio codec (libopus $OPUS_VERSION), https://opus-codec.org
    Built from source into the app by opusic-sys $(version opusic-sys); used to read Opus audio
    (.webm, .mkv, .opus files, e.g. from YouTube).

EOF
  cat "$OPUSIC/opus/COPYING"
  echo
  rule
  cat <<EOF

1b. ONNX Runtime ${ORT_DIR#licenses/onnxruntime-}, https://onnxruntime.ai
    Linked into the app by ort-sys $(version ort-sys); runs the speech model. Its licence,
    then the notices for the third-party code it contains.

EOF
  cat "$ORT_DIR/LICENSE"
  echo
  cat "$ORT_DIR/ThirdPartyNotices.txt"
  echo
  rule
  cat <<EOF

1c. Microsoft Edge WebView2 SDK: the WebView2 loader (Windows only)
    Linked into PoodPim.exe by webview2-com-sys $(version webview2-com-sys); opens the
    Settings window.

EOF
  cat licenses/webview2-sdk/LICENSE.txt
  echo
  cat licenses/webview2-sdk/NOTICE.txt
  echo
  rule
  echo "PART 2. RUST CRATES (macOS and Windows builds), grouped by licence text"
  cat "$CRATES"
} > THIRD-PARTY-NOTICES.txt
echo "wrote THIRD-PARTY-NOTICES.txt ($(grep -c '^  - ' THIRD-PARTY-NOTICES.txt) crate entries)"
