#!/bin/sh
# Removes PoodPim from this Mac: the app, its start-at-login entry, settings, logs and
# permissions, and, if you say so, the downloaded model (2.4 GB).
#
#   curl -fsSL https://raw.githubusercontent.com/thirawat/PoodPim/main/uninstall.sh | sh
#
# Options: --yes (no questions), --model / --keep-model (delete or keep the model; with
# --yes the default is to keep it). POODPIM_APP: remove only this PoodPim.app (the app sets
# it to itself); without it, /Applications/PoodPim.app and ~/Applications/PoodPim.app.
# PoodPim.app runs this script for its menu item "Uninstall PoodPim…".
set -u

YES=0
MODEL=ask
for a in "$@"; do
  case $a in
    --yes) YES=1 ;;
    --model) MODEL=yes ;;
    --keep-model) MODEL=no ;;
    *) printf 'unknown option %s\n' "$a" >&2; exit 2 ;;
  esac
done

# Questions go to the terminal: under `curl | sh` standard input is the script itself.
ask() {
  [ "$YES" = 1 ] && return 1
  printf '%s [y/N] ' "$1" >/dev/tty
  read -r answer </dev/tty || return 1
  case $answer in y | Y | yes | YES) return 0 ;; *) return 1 ;; esac
}

DATA="$HOME/Library/Application Support/poodpim"
if [ -n "${POODPIM_APP:-}" ]; then
  set -- "$POODPIM_APP"
else
  set -- /Applications/PoodPim.app "$HOME/Applications/PoodPim.app"
fi
if [ "$YES" != 1 ]; then
  ask "ลบ PoodPim ออกจากเครื่องนี้ (แอป, การตั้งค่า, การเปิดตอนเปิดเครื่อง)?" || { echo "ยกเลิก"; exit 0; }
fi
if [ "$MODEL" = ask ]; then
  MODEL=no
  if [ -d "$DATA/openthai-asr" ] &&
    ask "ลบโมเดลถอดเสียง ($(du -sh "$DATA/openthai-asr" | cut -f1)) ด้วยไหม? ถ้าเก็บไว้ ติดตั้งใหม่แล้วไม่ต้องดาวน์โหลดซ้ำ"; then
    MODEL=yes
  fi
fi

# Quit the app (it may be the one that started this) and stop it starting at login.
for app in "$@"; do
  exe=$app/Contents/MacOS/poodpim
  i=0
  while pgrep -f "$exe" >/dev/null 2>&1 && [ "$i" -lt 20 ]; do
    i=$((i + 1))
    [ "$i" -eq 10 ] && pkill -f "$exe"
    sleep 0.5
  done
done
launchctl bootout "gui/$(id -u)/local.poodpim.app" 2>/dev/null
rm -f "$HOME/Library/LaunchAgents/local.poodpim.app.plist"

status=0
for app in "$@"; do
  if [ -d "$app" ]; then
    rm -rf "$app" && echo "ลบ $app แล้ว" || { echo "ลบ $app ไม่ได้ (ลองลากไปที่ถังขยะเอง)" >&2; status=1; }
  fi
done

if [ "$MODEL" = yes ]; then
  rm -rf "$DATA" && echo "ลบการตั้งค่าและโมเดลแล้ว"
elif [ -d "$DATA" ]; then
  find "$DATA" -mindepth 1 -maxdepth 1 ! -name openthai-asr -exec rm -rf {} +
  echo "ลบการตั้งค่าแล้ว (เก็บโมเดลไว้ที่ $DATA/openthai-asr)"
fi
rm -rf "$HOME/Library/Logs/PoodPim"
# Forget the microphone and Accessibility permissions (best effort).
tccutil reset Microphone local.poodpim.app >/dev/null 2>&1
tccutil reset Accessibility local.poodpim.app >/dev/null 2>&1

[ "$status" = 0 ] && echo "ถอนการติดตั้ง PoodPim เรียบร้อย"
exit "$status"
