#!/bin/sh
# Build dist/PoodPim-<version>-mac.zip to give to other people: PoodPim.app preset to the
# local backend (it offers to download the model on first launch) and a Thai install guide.
# Neither a server URL nor an API key is in the zip (it may be published): to use a team's
# serverless backend, give them to each person separately (menu → Server URL & API key…).
set -eu
cd "$(dirname "$0")/.."

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
STAGE=dist/share/PoodPim
ZIP=dist/PoodPim-$VERSION-mac.zip
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

# The preset: local backend, no server URL or key.
cat > "$TMP/preset.json" <<'JSON'
{
  "backend": "local",
  "server_url": "",
  "api_key": "",
  "mixed_english": true,
  "restore_clipboard": true,
  "start_at_login": true
}
JSON

rm -rf dist/share "$ZIP"
APP="$STAGE/PoodPim.app" POODPIM_PRESET="$TMP/preset.json" ./scripts/bundle-macos.sh
cp scripts/share/install-th.txt "$STAGE/README-TH.txt"
ditto -c -k --keepParent "$STAGE" "$ZIP"
rm -rf dist/share
echo "Built $ZIP ($(du -h "$ZIP" | cut -f1))"
