#!/bin/sh
# Writes THIRD-PARTY-NOTICES.txt: the licences of the third-party code built into the desktop
# app that ask for their text to go with the app (libopus: BSD-3-Clause; Symphonia: MPL-2.0,
# with where its source is). Copied from the crates' own files, so run it again (and commit
# the result) when Cargo.lock moves symphonia, symphonia-adapter-libopus or opusic-sys.
# The packages ship the file: bundle-macos.sh, package-macos.sh, package-windows.sh.
set -eu
cd "$(dirname "$0")/.."

# name version of a package in Cargo.lock
version() { awk -v n="$1" '$0 == "name = \"" n "\"" { getline; gsub(/version = |"/, ""); print; exit }' Cargo.lock; }
REG=$(cargo metadata --format-version 1 >/dev/null && ls -d "${CARGO_HOME:-$HOME/.cargo}"/registry/src/index.crates.io-*/ | head -1)
crate() { echo "$REG$1-$(version "$1")"; }

OPUSIC=$(crate opusic-sys)
ADAPTER=$(crate symphonia-adapter-libopus)
SYMPHONIA=$(crate symphonia)
OPUS_VERSION=$(cat "$OPUSIC/opus/package_version" 2>/dev/null | sed -n 's/^PACKAGE_VERSION="\(.*\)"/\1/p')
SYMPHONIA_CRATES=$(awk '/^name = "symphonia/ { gsub(/name = |"/, ""); n = $0; getline; gsub(/version = |"/, ""); print "     - " n " " $0 }' Cargo.lock | grep -v adapter)

{
  cat <<EOF
PoodPim — third-party notices

PoodPim itself is under the MIT licence (LICENSE). The desktop app also contains the
software below, which asks for its licence to go with copies of the app.

================================================================================
1. Opus audio codec (libopus ${OPUS_VERSION:-}), https://opus-codec.org
   Built from source into PoodPim by opusic-sys $(version opusic-sys)
   (https://github.com/DoumanAsh/opusic-sys, same licence text as below).
   Used to read Opus audio (.webm, .mkv, .opus files, e.g. from YouTube).
================================================================================

EOF
  cat "$OPUSIC/opus/COPYING"
  cat <<EOF

================================================================================
2. symphonia-adapter-libopus $(version symphonia-adapter-libopus)
   https://github.com/aschey/symphonia-adapters
   Licensed under MIT OR Apache-2.0; used here under the MIT licence.
================================================================================
EOF
  cat "$ADAPTER/LICENSE-MIT"
  cat <<EOF

================================================================================
3. Symphonia, https://github.com/pdeljanov/Symphonia
   Copyright (c) 2019-2026 The Project Symphonia Developers.
   Used to read audio and video files (AAC, ALAC, MP3, MP4/MOV, MKV/WebM, Ogg).
   These crates, unmodified, are licensed under the Mozilla Public License 2.0:
${SYMPHONIA_CRATES}
   Their source code is available from https://crates.io/crates/<name>/<version>
   (for example https://crates.io/crates/symphonia/$(version symphonia)) and from the
   repository above. The licence text follows.
================================================================================

EOF
  cat "$SYMPHONIA/LICENSE"
} > THIRD-PARTY-NOTICES.txt
echo "wrote THIRD-PARTY-NOTICES.txt"
