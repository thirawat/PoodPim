#!/bin/sh
# Build dist/PoodPim.app: a menu-bar app (no Dock icon) with its own microphone and
# Accessibility permission entries.
#
# Signed with a self-signed "PoodPim Local Signing" certificate kept in its own keychain
# (~/Library/Keychains/poodpim-signing.keychain-db, created on first run). macOS ties the
# Accessibility permission to the signature, so with a fixed certificate the permission
# survives rebuilds (an ad-hoc signature changes every build and loses it).
set -eu
cd "$(dirname "$0")/.."

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
# APP: output path. POODPIM_PRESET: a config file to ship as the first-run settings
# (scripts/package-macos.sh uses both).
APP=${APP:-dist/PoodPim.app}

cargo build --release
rm -rf "$APP"
mkdir -p "$(dirname "$APP")" "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/poodpim "$APP/Contents/MacOS/poodpim"
python3 scripts/make_icon.py "$APP/Contents/Resources/AppIcon.icns"
# The installer and uninstaller, which the app runs to update or remove itself.
cp install.sh uninstall.sh "$APP/Contents/Resources/"
if [ -n "${POODPIM_PRESET:-}" ]; then
  cp "$POODPIM_PRESET" "$APP/Contents/Resources/preset.json"
fi

cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>PoodPim</string>
  <key>CFBundleDisplayName</key><string>PoodPim</string>
  <key>CFBundleIdentifier</key><string>local.poodpim.app</string>
  <key>CFBundleExecutable</key><string>poodpim</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>CFBundleVersion</key><string>${VERSION}</string>
  <key>LSMinimumSystemVersion</key><string>13.4</string>
  <key>LSUIElement</key><true/>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSMicrophoneUsageDescription</key>
  <string>PoodPim listens while you hold the dictation key and types what you say.</string>
</dict>
</plist>
EOF

IDENTITY="PoodPim Local Signing"
KC="$HOME/Library/Keychains/poodpim-signing.keychain-db"
KCPW=poodpim-local # protects only this local signing key
if [ ! -f "$KC" ]; then
  TMP=$(mktemp -d)
  printf '%s\n' '[req]' 'distinguished_name = dn' 'prompt = no' 'x509_extensions = ext' \
    '[dn]' "CN = $IDENTITY" '[ext]' 'basicConstraints = critical, CA:false' \
    'keyUsage = critical, digitalSignature' 'extendedKeyUsage = critical, codeSigning' > "$TMP/cert.cnf"
  /usr/bin/openssl req -x509 -newkey rsa:2048 -nodes -days 3650 -config "$TMP/cert.cnf" \
    -keyout "$TMP/key.pem" -out "$TMP/cert.pem" 2>/dev/null
  /usr/bin/openssl pkcs12 -export -inkey "$TMP/key.pem" -in "$TMP/cert.pem" -passout pass:x -out "$TMP/id.p12"
  security create-keychain -p "$KCPW" "$KC"
  security set-keychain-settings "$KC" # no auto-lock
  security unlock-keychain -p "$KCPW" "$KC"
  security import "$TMP/id.p12" -k "$KC" -P x -T /usr/bin/codesign >/dev/null
  security set-key-partition-list -S apple-tool:,apple: -s -k "$KCPW" "$KC" >/dev/null
  rm -rf "$TMP"
fi
security unlock-keychain -p "$KCPW" "$KC"
# By hash: the self-signed certificate isn't trusted, so codesign won't look it up by name.
HASH=$(security find-identity -p codesigning "$KC" | awk -v n="\"$IDENTITY\"" '$0 ~ n {print $2; exit}')
# codesign only finds identities in keychains on the search list: add ours while signing.
OLD_LIST=$(security list-keychains -d user | tr -d '"')
restore_list() { security list-keychains -d user -s $OLD_LIST; }
trap restore_list EXIT
security list-keychains -d user -s $OLD_LIST "$KC"
codesign --force --keychain "$KC" --sign "$HASH" --identifier local.poodpim.app "$APP"
restore_list
trap - EXIT
echo "Built $APP ($(du -sh "$APP" | cut -f1))"
