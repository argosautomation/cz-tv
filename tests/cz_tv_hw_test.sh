#!/usr/bin/env bash
# Hardware test for cz-tv 1.0. Runs ON the card.
#
# Drives the real app through a uinput keyboard and grabs /dev/fb0 on a
# timeline, so every claim about the guide, the ON NOW line, type-to-search and
# on-panel playback is read off the hardware rather than reasoned about.
#
# Timeline: home -> ENTER into All channels -> type "dw" -> ENTER to play DW
# English on the card's own screen -> ESC back -> ESC ESC quit.
set -u
OUT=/tmp/cztv_test
rm -rf "$OUT"; mkdir -p "$OUT"
cd "$OUT"
APP=/usr/share/APPLaunch/bin/cz-tv

echo "hdmi: $(cat /sys/class/drm/card0-HDMI-A-1/status)"
echo "mem:  $(awk '/MemAvailable/{print $2/1024" MiB avail"}' /proc/meminfo)"

sudo python3 /tmp/inject_keys.py SLEEP:5 ENTER SLEEP:4 dw SLEEP:4 ENTER \
  SLEEP:26 ESC SLEEP:4 ESC SLEEP:2 ESC SLEEP:2 ESC SLEEP:2 \
  >"$OUT/inject.json" 2>"$OUT/inject.err" &
INJ=$!
NODE=""
for i in $(seq 1 40); do
  NODE=$(python3 -c "import json;print(json.load(open('$OUT/inject.json'))['node'])" 2>/dev/null)
  [ -n "$NODE" ] && break
  sleep 0.5
done
if [ -z "$NODE" ]; then echo "FAIL: no uinput node"; cat "$OUT/inject.err"; exit 1; fi
echo "kbd: $NODE"
sudo chmod a+r "$NODE"

HOME="${HOME:-/home/$(id -un)}" CZ_KBD="$NODE" XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-1 \
  "$APP" >"$OUT/app.log" 2>&1 &
APPPID=$!
T0=$(date +%s.%N)

grab() {
  head -c 108800 /dev/fb0 > "$OUT/$1.raw" 2>/dev/null
  echo "[$(printf %.1f "$(echo "$(date +%s.%N)-$T0" | bc)")s] $1  backlight=$(cat /sys/class/backlight/backlight/brightness)"
}
snap() {
  { echo "--- $1 at $(printf %.1f "$(echo "$(date +%s.%N)-$T0" | bc)")s"
    # The stitcher URL carries a random session id, nothing of the viewer's,
    # but keep the log tidy and short anyway.
    pgrep -a -f 'ffmpeg|vlc' | grep -v pgrep | sed -E 's#(https?://)[^ ]+#\1<stream-url>#g'
    # /usr/bin/time is not installed on the card, so peak RSS comes from the
    # kernel's own high-water mark instead.
    grep -E "^(VmRSS|VmHWM)" "/proc/$APPPID/status" 2>/dev/null | tr -s ' ' 
  } >> "$OUT/procs.txt"
  echo "[snap $1] $(pgrep -c -f 'ffmpeg' || true) ffmpeg, $(pgrep -c -f 'vlc' || true) vlc"
}
at() {
  local target=$1
  while :; do
    local now; now=$(echo "$(date +%s.%N)-$T0" | bc)
    (( $(echo "$now >= $target" | bc) )) && break
    sleep 0.3
  done
}

at 3;  grab 01_home;            snap home
at 8;  grab 02_all_channels
at 12; grab 03_search_dw
at 20; grab 04_playing;         snap playing
at 32; grab 05_playing_later;   snap playing_late
at 42; grab 06_back_in_guide
at 50; grab 07_end

wait $INJ 2>/dev/null
sleep 1
if kill -0 $APPPID 2>/dev/null; then echo "app still running, killing"; kill $APPPID 2>/dev/null; fi
# fbde[v] on purpose: over ssh the remote bash -c command line contains this
# pattern itself, and 'fbdev' would match it and kill the test's own shell.
pkill -f 'ffmpeg .*fbde[v]' 2>/dev/null
pkill -f cvl[c] 2>/dev/null
echo "=== app.log ==="; cat "$OUT/app.log"
echo "=== peak rss (VmHWM, last sample) ==="; grep VmHWM "$OUT/procs.txt" | tail -1
echo "=== procs ==="; cat "$OUT/procs.txt"
