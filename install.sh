#!/bin/sh
# Installs or updates PoodPim on a Mac with Apple silicon (macOS 13.4 or later):
#
#   curl -fsSL https://raw.githubusercontent.com/thirawat/PoodPim/main/install.sh | sh
#
# A given version, e.g. a release candidate:
#   curl -fsSL https://raw.githubusercontent.com/thirawat/PoodPim/main/install.sh | POODPIM_VERSION=v1.0.2-rc.1 sh
#
# It downloads the release zip from GitHub, checks its SHA-256 and that the app is signed
# with the PoodPim certificate, puts PoodPim.app in /Applications (or ~/Applications when
# /Applications isn't writable), quits a running copy and opens the new one. Files that
# curl downloads aren't quarantined, so macOS doesn't ask to "Open Anyway".
#
# PoodPim.app carries this script and runs it to update itself (POODPIM_APP = the app to
# replace). Uninstall: uninstall.sh.
set -eu

REPO=thirawat/PoodPim
# The certificate every release is signed with (scripts/bundle-macos.sh). If the signing
# key ever changes, update this, or installs and updates will refuse the new app.
CERT_LEAF='H"1a848cc393eaa15e9d0a81fa246f02affa40ca52"'

say() { printf '%s\n' "$*"; }
fail() {
  say "ติดตั้ง PoodPim ไม่สำเร็จ: $*" >&2
  exit 1
}
leaf() { codesign -d -r- "$1" 2>&1 | sed -n 's/.*certificate leaf = \(H"[0-9a-f]*"\).*/\1/p'; }

[ "$(uname -s)" = Darwin ] || fail "ใช้ได้เฉพาะ macOS"
[ "$(sysctl -n hw.optional.arm64 2>/dev/null || echo 0)" = 1 ] ||
  fail "PoodPim ใช้ได้กับ Mac ชิป Apple (M1 ขึ้นไป) เท่านั้น เครื่องนี้เป็นชิป Intel"
OS=$(sw_vers -productVersion)
MAJOR=${OS%%.*}
MINOR=$(printf '%s' "$OS" | cut -d. -f2)
if [ "$MAJOR" -lt 13 ] || { [ "$MAJOR" -eq 13 ] && [ "${MINOR:-0}" -lt 4 ]; }; then
  fail "ต้องใช้ macOS 13.4 ขึ้นไป (เครื่องนี้ $OS)"
fi

TAG=${POODPIM_VERSION:-}
if [ -z "$TAG" ]; then
  # github.com/<repo>/releases/latest redirects to …/releases/tag/<newest stable tag>.
  TAG=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest") ||
    fail "ติดต่อ GitHub ไม่ได้ ตรวจการเชื่อมต่ออินเทอร์เน็ต"
  TAG=${TAG##*/}
fi
case $TAG in v[0-9]*) ;; *) fail "หาเวอร์ชันล่าสุดไม่เจอ ($TAG)" ;; esac
VERSION=${TAG#v}
ZIP=PoodPim-$VERSION-mac.zip
URL=https://github.com/$REPO/releases/download/$TAG

if [ -n "${POODPIM_APP:-}" ]; then
  APP=$POODPIM_APP
elif [ -d /Applications/PoodPim.app ] || { [ -w /Applications ] && [ ! -d "$HOME/Applications/PoodPim.app" ]; }; then
  APP=/Applications/PoodPim.app
else
  APP=$HOME/Applications/PoodPim.app
fi
DIR=$(dirname "$APP")
mkdir -p "$DIR"
[ -w "$DIR" ] || fail "ไม่มีสิทธิ์เขียนโฟลเดอร์ $DIR (ต้องใช้บัญชีผู้ดูแลเครื่อง)"

TMP=$(mktemp -d)
STAGE=$DIR/.PoodPim-install-$$
# Started by the app to update it: the app has quit already, so whatever happens, end with
# PoodPim running again.
REOPEN=0
[ -n "${POODPIM_APP:-}" ] && REOPEN=1
DONE=0
cleanup() {
  rm -rf "$TMP" "$STAGE"
  # The old app was quit: open it again if the new one didn't go in.
  if [ "$REOPEN" = 1 ] && [ "$DONE" = 0 ] && [ -d "$APP" ]; then open "$APP" || true; fi
}
trap cleanup EXIT

say "กำลังดาวน์โหลด PoodPim ${VERSION}…"
curl -fL --progress-bar -o "$TMP/$ZIP" "$URL/$ZIP" || fail "ดาวน์โหลด $ZIP ไม่ได้"
curl -fsSL -o "$TMP/$ZIP.sha256" "$URL/$ZIP.sha256" || fail "ดาวน์โหลด checksum ไม่ได้"
(cd "$TMP" && shasum -a 256 -c "$ZIP.sha256" >/dev/null) || fail "ไฟล์ที่ดาวน์โหลดเสียหาย (checksum ไม่ตรง)"

mkdir -p "$STAGE"
ditto -x -k "$TMP/$ZIP" "$STAGE"
NEW=$STAGE/PoodPim/PoodPim.app
[ -d "$NEW" ] || fail "ในไฟล์ zip ไม่มี PoodPim.app"
codesign --verify --strict "$NEW" 2>/dev/null || fail "ลายเซ็นของแอปไม่ถูกต้อง"
[ "$(leaf "$NEW")" = "$CERT_LEAF" ] || fail "แอปไม่ได้เซ็นด้วยใบรับรองของ PoodPim"

# Quit the running copy (it may be the one that started this update) and wait for it.
EXE=$APP/Contents/MacOS/poodpim
if pgrep -f "$EXE" >/dev/null 2>&1; then
  REOPEN=1
  say "กำลังปิด PoodPim ตัวเดิม…"
  i=0
  while pgrep -f "$EXE" >/dev/null 2>&1; do
    i=$((i + 1))
    [ "$i" -eq 20 ] && pkill -f "$EXE" || true
    [ "$i" -gt 40 ] && fail "ปิด PoodPim ตัวเดิมไม่ได้"
    sleep 0.5
  done
fi

[ -e "$APP" ] && mv "$APP" "$STAGE/old.app"
mv "$NEW" "$APP"
DONE=1
xattr -dr com.apple.quarantine "$APP" 2>/dev/null || true
open "$APP"
say "ติดตั้ง PoodPim $VERSION ที่ $APP แล้ว"
say "ดูไอคอนไมโครโฟน 🎙 ที่แถบเมนูด้านบนขวา แล้วกด Option ขวาค้างไว้เพื่อพูด"
