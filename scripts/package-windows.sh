#!/bin/sh
# Build dist/PoodPim-<version>-windows.zip from this Mac: cross-compiles PoodPim.exe with
# cargo-xwin (`cargo install --locked cargo-xwin`; it downloads the MSVC CRT and Windows SDK
# libraries into target/xwin-cache and accepts Microsoft's license for them; libopus is built
# with CMake, which also needs `brew install cmake ninja` and `rustup component add llvm-tools`),
# and ships it with
#   - DirectML.dll: the prebuilt ONNX Runtime for Windows always links DirectML, and the
#     copy in older Windows versions may be too old, so the redistributable goes next to the exe;
#   - preset.json: local backend, no server URL or API key (see package-macos.sh);
#   - README-TH.txt: Thai install guide;
#   - PoodPim.ico: Start menu shortcut and Settings > Apps icon;
#   - THIRD-PARTY-NOTICES.txt: licences of libopus and Symphonia (scripts/make-notices.sh).
# Also writes <zip>.sha256, which install.ps1 and the app's updater check the download against.
# No PowerShell scripts: the app updates and uninstalls itself.
# The exe also needs the Visual C++ runtime (msvcp140.dll), present on most PCs; the guide
# links the installer.
set -eu
cd "$(dirname "$0")/.."

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
TARGET=x86_64-pc-windows-msvc
export XWIN_CACHE_DIR="${XWIN_CACHE_DIR:-$PWD/target/xwin-cache}"
export XWIN_ACCEPT_LICENSE=1
DML_VERSION=1.15.4
DML_SHA256=9c9e6d822561c6c41b90e6994b3e8857cf1d66dbfb1e0c4c799c7c89b4e92da1
DML_DIR="$XWIN_CACHE_DIR/directml-$DML_VERSION"
STAGE=dist/share-windows/PoodPim
ZIP=dist/PoodPim-$VERSION-windows.zip

cargo xwin build --release --target "$TARGET"

if [ ! -f "$DML_DIR/bin/x64-win/DirectML.dll" ]; then
  mkdir -p "$DML_DIR"
  curl -fsSL -o "$DML_DIR/directml.nupkg" \
    "https://api.nuget.org/v3-flatcontainer/microsoft.ai.directml/$DML_VERSION/microsoft.ai.directml.$DML_VERSION.nupkg"
  (cd "$DML_DIR" && unzip -o -q directml.nupkg 'bin/x64-win/DirectML.dll' LICENSE.txt && rm directml.nupkg)
fi
echo "$DML_SHA256  $DML_DIR/bin/x64-win/DirectML.dll" | shasum -a 256 -c - >/dev/null

rm -rf dist/share-windows "$ZIP"
mkdir -p "$STAGE"
cp "target/$TARGET/release/poodpim.exe" "$STAGE/PoodPim.exe"
cp "$DML_DIR/bin/x64-win/DirectML.dll" "$STAGE/"
cp "$DML_DIR/LICENSE.txt" "$STAGE/LICENSE-DirectML.txt"
cp scripts/share/install-th-windows.txt "$STAGE/README-TH.txt"
cp THIRD-PARTY-NOTICES.txt "$STAGE/"
cp assets/PoodPim.ico "$STAGE/"
cat > "$STAGE/preset.json" <<'JSON'
{
  "backend": "local",
  "server_url": "",
  "api_key": "",
  "mixed_english": true,
  "restore_clipboard": true,
  "start_at_login": true
}
JSON
(cd dist/share-windows && python3 -m zipfile -c "../$(basename "$ZIP")" PoodPim)
rm -rf dist/share-windows
(cd dist && shasum -a 256 "$(basename "$ZIP")" > "$(basename "$ZIP").sha256")
echo "Built $ZIP ($(du -h "$ZIP" | cut -f1)) and $ZIP.sha256"
