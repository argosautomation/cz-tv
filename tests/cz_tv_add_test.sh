#!/usr/bin/env bash
# End-to-end test of "Add channels", with no card, no root and no network.
#
# It builds the binary for THIS machine and drives the real app through
# tests/cz_tv_ui_test.py - a file standing in for the framebuffer and a FIFO
# standing in for the keypad, both pointed at with the app's own environment
# variables. So the code under test is the whole of it: the key decode, the
# menu, the editor, the fetch, the decode check and the write.
#
# The assertion is not a screenshot. It is the file the app wrote: a channel
# that reached ~/.config/cz-tv/channels.d/ is a channel that will be in the
# guide on the next launch, which is the entire point of the feature.
#
#   tests/cz_tv_add_test.sh            # and look at $OUT/*/montage.png
set -euo pipefail
cd "$(dirname "$0")/.."
OUT=${OUT:-/tmp/cz-tv-add-test}
if [ -n "${BIN:-}" ]; then BIN_SET=1; fi
BIN=${BIN:-target/debug/cz-tv}
rm -rf "$OUT"; mkdir -p "$OUT/serve" "$OUT/seed/Downloads"

# Always rebuild, unless the caller named a binary. An -x test passes on a
# binary from an earlier commit, and a stale one made this suite green against
# code that no longer exists.
if [ -z "${BIN_SET:-}" ]; then cargo build; fi

# A stream to point at, served from here, so the decode check in --add has
# something real to answer it and the test needs nothing off the internet.
PORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')
cat > "$OUT/serve/live.m3u8" <<EOF
#EXTM3U
#EXT-X-VERSION:3
#EXT-X-TARGETDURATION:6
#EXT-X-MEDIA-SEQUENCE:1
#EXTINF:6.0,
seg1.ts
EOF
( cd "$OUT/serve" && exec python3 -m http.server "$PORT" --bind 127.0.0.1 ) >"$OUT/http.log" 2>&1 &
HTTP=$!
trap 'kill $HTTP 2>/dev/null || true' EXIT
for _ in $(seq 20); do
  curl -fsS -o /dev/null "http://127.0.0.1:$PORT/live.m3u8" 2>/dev/null && break
  sleep 0.25
done

# A playlist where the Add screen will find it: this is the USB-stick case, and
# the pack case, and the scp case, all of which are "a .m3u already on the card".
cat > "$OUT/seed/Downloads/barn-cams.m3u" <<EOF
#EXTM3U
# Barn cameras
#EXTINF:-1 group-title="Cameras",Front Barn
http://127.0.0.1:$PORT/live.m3u8
#EXTINF:-1 group-title="Cameras",Back Field
http://127.0.0.1:$PORT/live.m3u8?cam=2
EOF

drive() { # drive <name> <keys...>
  local name=$1; shift
  python3 tests/cz_tv_ui_test.py --bin "$BIN" --out "$OUT/$name" --seed "$OUT/seed" "$@" \
    > "$OUT/$name.log" 2>&1 || { echo "FAIL: driver died in $name"; cat "$OUT/$name.log"; exit 1; }
  if [ -s "$OUT/$name/app.log" ]; then
    echo "FAIL: $name - the app printed something:"; cat "$OUT/$name/app.log"; exit 1
  fi
}
check() { # check <name> <file> <string>
  if ! grep -qF -- "$3" "$OUT/$1/home/.config/cz-tv/$2" 2>/dev/null; then
    echo "FAIL: $1 - '$3' is not in $2"
    ls -l "$OUT/$1/home/.config/cz-tv/channels.d/" 2>/dev/null || echo "  (channels.d does not exist)"
    exit 1
  fi
  echo "ok: $1"
}

# 1. One channel out of a playlist on the card. END selects the Add row at the
#    bottom of the home screen, then the file, then the channel inside it.
drive one_channel SLEEP:3 GRAB:01_home END ENTER SLEEP:1 GRAB:02_menu DOWN ENTER SLEEP:1 \
  GRAB:03_browse DOWN ENTER SLEEP:3 GRAB:04_added ENTER SLEEP:3 GRAB:05_guide
check one_channel channels.d/my-channels.m3u "Front Barn"

# 2. The whole playlist, from an EMPTY guide - the state a stranger is in when
#    the built-in lineup has been turned off or every source has failed.
mkdir -p "$OUT/seed/.config/cz-tv"
echo '{"include_builtin": false}' > "$OUT/seed/.config/cz-tv/config.json"
drive whole_list SLEEP:2.5 GRAB:01_empty ENTER SLEEP:1 DOWN ENTER SLEEP:1 GRAB:02_browse \
  ENTER SLEEP:3 GRAB:03_added ENTER SLEEP:3 GRAB:04_guide
check whole_list channels.d/barn-cams.m3u "Back Field"
rm -f "$OUT/seed/.config/cz-tv/config.json"

# 3. An address typed in, then named. TAB is the keypad's only way to a capital
#    letter, so the B in "Barn cam" is the thing being tested as much as the URL.
drive typed SLEEP:3 END ENTER SLEEP:1 ENTER SLEEP:0.8 GRAB:01_editor \
  BKSP BKSP BKSP BKSP "://127.0.0.1:$PORT/live.m3u8" GRAB:02_typed ENTER SLEEP:4 \
  GRAB:03_name BKSP BKSP BKSP BKSP BKSP BKSP BKSP BKSP BKSP BKSP BKSP BKSP BKSP BKSP BKSP BKSP BKSP BKSP BKSP BKSP \
  TAB b TAB "arn cam" GRAB:04_named ENTER SLEEP:4 GRAB:05_added ENTER SLEEP:3 GRAB:06_guide
check typed channels.d/my-channels.m3u "Barn cam"
check typed channels.d/my-channels.m3u "http://127.0.0.1:$PORT/live.m3u8"

# 4. ESC out of the Add screen writes nothing at all.
drive cancelled SLEEP:3 END ENTER SLEEP:1 ESC SLEEP:0.5 GRAB:01_back ESC ESC
if [ -e "$OUT/cancelled/home/.config/cz-tv/channels.d" ]; then
  echo "FAIL: cancelled - backing out of the Add screen still wrote something"; exit 1
fi
echo "ok: cancelled"

# 5. A command line that was not understood never reaches the panel. This is
#    the regression for the real incident: `cz-tv --bogus-flag` used to fall
#    through to the full-screen guide, so on a card somebody was holding, a
#    typo took over the screen and started playing a channel over what they
#    were doing. The assertion is the framebuffer file: it must be untouched.
mkdir -p "$OUT/args"
FB="$OUT/args/fb.raw"
: > "$FB"
for BAD in --bogus-flag --Add -x junk; do
  set +e
  OUTPUT=$(APPLAUNCH_LINUX_FBDEV_DEVICE="$FB" CZ_KBD=/dev/null HOME="$OUT/args" \
           "$BIN" "$BAD" 2>&1 >/dev/null)
  RC=$?
  set -e
  if [ "$RC" -ne 2 ]; then
    echo "FAIL: args - '$BAD' exited $RC, expected 2"; exit 1
  fi
  case "$OUTPUT" in
    *"unknown option"*) : ;;
    *) echo "FAIL: args - '$BAD' said '$OUTPUT', expected an unknown-option error"; exit 1 ;;
  esac
  if [ -s "$FB" ]; then
    echo "FAIL: args - '$BAD' wrote $(stat -c%s "$FB") bytes to the framebuffer"; exit 1
  fi
done
# And the ones that ARE commands still work, off the screen.
APPLAUNCH_LINUX_FBDEV_DEVICE="$FB" HOME="$OUT/args" "$BIN" --version >/dev/null
APPLAUNCH_LINUX_FBDEV_DEVICE="$FB" HOME="$OUT/args" "$BIN" --help >/dev/null
APPLAUNCH_LINUX_FBDEV_DEVICE="$FB" HOME="$OUT/args" "$BIN" --sources >/dev/null
if [ -s "$FB" ]; then
  echo "FAIL: args - a shell command drew on the framebuffer"; exit 1
fi
# A failed --add names the whole address, not a 48-char stub: in a shell that
# line is the only copy of what was tried.
LONG="http://127.0.0.1:1/a/very/long/path/that/keeps/going/and/going/and/going/master.m3u8"
set +e
ADDOUT=$(HOME="$OUT/args" "$BIN" --add "$LONG" "Nope" Test 2>&1)
set -e
case "$ADDOUT" in
  *"$LONG"*) : ;;
  *) echo "FAIL: args - the failure line truncated the address: $ADDOUT"; exit 1 ;;
esac
echo "ok: args"

echo
echo "PASS - 5 scenarios. Screens: $OUT/*/montage.png"
