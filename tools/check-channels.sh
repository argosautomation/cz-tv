#!/usr/bin/env bash
# Fetch every channel in a playlist, DECODE A FRAME, and build a contact sheet
# so a human can look at the pictures.
#
#   tools/check-channels.sh [-o OUTDIR] [-j JOBS] [file.m3u ...]
#
# Needs: curl, ffmpeg (ffprobe). ImageMagick is used if present, for a blank-
# frame warning.
#
# Why this exists, and why a status code is not enough: this app began as a
# Pluto TV client. Pluto now answers every client profile with a "Pluto TV is
# no longer available on this device" slate, and that slate is clean H.264 +
# AAC delivered with an HTTP 200 - so a reachability check, a codec probe and a
# playlist parse ALL PASS on a channel that shows a dead card. The same sweep
# found four channels serving 200 on the master playlist and 404 on every
# variant inside it. Only a decoded picture, looked at, catches either.
#
# Output: OUTDIR/report.tsv, OUTDIR/frames/*.png, OUTDIR/contact-sheet.jpg
set -uo pipefail

UA="CardputerZero-TV/1.0"
OUT=check-out
JOBS=6
while getopts "o:j:h" o; do
  case $o in
    o) OUT=$OPTARG ;;
    j) JOBS=$OPTARG ;;
    h) sed -n '2,20p' "$0"; exit 0 ;;
    *) exit 2 ;;
  esac
done
shift $((OPTIND-1))
files=("$@")
if [[ ${#files[@]} -eq 0 ]]; then
  mapfile -t files < <(ls channels/*.m3u channels/packs/*.m3u 2>/dev/null)
fi
[[ ${#files[@]} -eq 0 ]] && { echo "no playlists given"; exit 2; }
command -v ffmpeg >/dev/null || { echo "need ffmpeg"; exit 2; }

mkdir -p "$OUT/frames"
: > "$OUT/report.tsv"

# Resolve a variant URI against the master's URL, removing dot segments the way
# RFC 3986 says to. Several CDNs publish variants as "../../../manifest/..." and
# answer 403 to a path that still contains a "..", so a naive string join turns
# a live channel into a dead one.
resolve_url() {
  local base=$1 rel=$2 origin path merged out=() seg
  case $rel in
    http://*|https://*) printf '%s' "$rel"; return ;;
  esac
  local relq=""
  case $rel in *\?*) relq="?${rel#*\?}"; rel="${rel%%\?*}" ;; esac
  base=${base%%\?*}
  origin=$(sed -E 's#^([a-z]+://[^/]+).*#\1#' <<<"$base")
  path=${base#"$origin"}
  if [[ $rel == /* ]]; then
    merged=$rel
  else
    merged="${path%/*}/$rel"
  fi
  local IFS=/
  read -ra seg <<<"${merged#/}"
  for s in "${seg[@]}"; do
    case $s in
      .) ;;
      ..) [[ ${#out[@]} -gt 0 ]] && unset "out[$((${#out[@]}-1))]" && out=("${out[@]}") ;;
      *) out+=("$s") ;;
    esac
  done
  printf '%s/%s%s' "$origin" "${out[*]}" "$relq"
}
export -f resolve_url

# ---------------------------------------------------------------- one channel
# Exported so xargs can run it in parallel; writes one TSV line and one PNG.
check_one() {
  local name=$1 group=$2 url=$3 idx=$4
  local slug; slug=$(tr -cs '[:alnum:]' '-' <<<"${name,,}" | sed 's/^-//;s/-$//')
  local png="$OUT/frames/$(printf '%02d' "$idx")-$slug.png"
  local body variant status="" codec="" res=""

  body=$(curl -sS --fail -L --max-time 15 -A "$UA" "$url" 2>/dev/null)
  if [[ -z $body ]]; then
    printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$idx" "$name" "$group" "NO-PLAYLIST" "" "$url" >> "$OUT/report.tsv"
    return
  fi
  if grep -q 'EXT-X-STREAM-INF' <<<"$body"; then
    # The card plays the SMALLEST rendition on its own panel, so that is the one
    # to test: a channel can have a working 1080p variant and a dead 360p one.
    local rel
    rel=$(awk '
      # DW serves its master with CRLF line endings, and a carriage return left
      # on the end of a variant URI is a 404 that looks exactly like a dead
      # channel.
      { sub(/\r$/, "") }
      /^#EXT-X-STREAM-INF/ {
        bw=0; res=0
        if (match($0, /BANDWIDTH=[0-9]+/))      { bw  = substr($0, RSTART+10, RLENGTH-10) + 0 }
        if (match($0, /RESOLUTION=[0-9]+x[0-9]+/)) {
          r = substr($0, RSTART+11, RLENGTH-11); split(r, d, /[xX]/); res = d[1] * d[2]
        }
        score = (res > 0 ? res : bw); pend = 1; next
      }
      /^[[:space:]]*$/ || /^#/ { next }
      pend == 1 { if (best == "" || score < bestscore) { bestscore = score; best = $0 } ; pend = 0 }
      END { print best }' <<<"$body")
    if [[ -z $rel ]]; then
      variant=$url
    else
      variant=$(resolve_url "$url" "$rel")
    fi
  else
    variant=$url
  fi

  # One frame, with a hard timeout: a stalled HLS fetch must not hang the sweep.
  if timeout 40 ffmpeg -hide_banner -loglevel error -y -user_agent "$UA" \
       -i "$variant" -frames:v 1 -vf scale=320:-2 "$png" </dev/null >/dev/null 2>"$png.err"; then
    if [[ -s $png ]]; then
      status=OK
      read -r codec res < <(ffprobe -v error -select_streams v:0 \
        -show_entries stream=codec_name,width,height -of csv=p=0 "$variant" 2>/dev/null \
        | awk -F, '{print $1" "$2"x"$3}')
      # A single flat colour is what a takedown slate, a black feed and a
      # colour-bar placeholder all look like. Not a failure - a LOOK-AT-ME.
      if command -v identify >/dev/null; then
        local sd; sd=$(identify -format '%[standard-deviation]' "$png" 2>/dev/null)
        if [[ -n $sd ]] && awk -v s="$sd" 'BEGIN{exit !(s < 900)}'; then status="FLAT?"; fi
      fi
    else
      status=NO-FRAME
    fi
  else
    status=DECODE-FAIL
    head -c 200 "$png.err" | tr '\n' ' ' > "$png.msg"
  fi
  printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$idx" "$name" "$group" "$status" "${codec:-} ${res:-}" "$variant" >> "$OUT/report.tsv"
  printf '%-4s %-28s %-12s %s\n' "$idx" "${name:0:28}" "$status" "${codec:-} ${res:-}"
}
export -f check_one
export OUT UA

# ---------------------------------------------------------------- the playlist
idx=0
tasks=$(mktemp)
for f in "${files[@]}"; do
  name=""; group=""
  while IFS= read -r raw || [[ -n $raw ]]; do
    line=${raw%$'\r'}
    [[ -z ${line// /} ]] && continue
    if [[ $line == '#EXTINF:'* ]]; then
      name=${line#*,}
      group=$(sed -nE 's/.*group-title="([^"]*)".*/\1/p' <<<"$line")
      continue
    fi
    [[ $line == '#'* ]] && continue
    idx=$((idx+1))
    printf '%s\t%s\t%s\t%s\n' "${name:-$line}" "${group:-Channels}" "$line" "$idx" >> "$tasks"
    name=""; group=""
  done < "$f"
done
echo "checking $idx channels, $JOBS at a time"
printf '%s\n' "idx	name	group	status	stream	url" > "$OUT/report.tsv"
# shellcheck disable=SC2016
xargs -a "$tasks" -d '\n' -P "$JOBS" -I{} bash -c 'IFS=$'"'"'\t'"'"' read -r n g u i <<<"{}"; check_one "$n" "$g" "$u" "$i"'
rm -f "$tasks"

# ---------------------------------------------------------------- contact sheet
# One JPEG of every frame, labelled, because the whole point is that somebody
# LOOKS at these. ffmpeg's tile filter, so ImageMagick stays optional.
mapfile -t frames < <(ls "$OUT"/frames/*.png 2>/dev/null | sort)
if [[ ${#frames[@]} -gt 0 ]]; then
  if command -v montage >/dev/null; then
    # Labelled, so a wrong picture can be named without counting tiles.
    labels=()
    for f in "${frames[@]}"; do
      labels+=(-label "$(basename "$f" .png | sed 's/^[0-9]*-//' | tr '-' ' ')" "$f")
    done
    montage "${labels[@]}" -tile 4x -geometry 320x180+3+3 -background '#111' \
      -fill white -pointsize 13 "$OUT/contact-sheet.jpg" 2>/dev/null
  else
    # ffmpeg only: same grid, no captions. tile= over a glob keeps this to one
    # filter rather than a per-input layout expression.
    ffmpeg -hide_banner -loglevel error -y -pattern_type glob -i "$OUT/frames/*.png" \
      -vf "scale=320:180:force_original_aspect_ratio=decrease,pad=320:180:(ow-iw)/2:(oh-ih)/2,tile=4x$(( (${#frames[@]} + 3) / 4 ))" \
      -frames:v 1 "$OUT/contact-sheet.jpg" 2>/dev/null
  fi
  [[ -s "$OUT/contact-sheet.jpg" ]] && \
    echo "contact sheet: $OUT/contact-sheet.jpg (${#frames[@]} frames - LOOK AT IT, a slate passes every other check)"
fi

echo
echo "report: $OUT/report.tsv"
awk -F'\t' 'NR>1 && $4!="OK" {print "  "$4"\t"$2}' "$OUT/report.tsv" | sort
bad=$(awk -F'\t' 'NR>1 && $4!="OK"' "$OUT/report.tsv" | wc -l)
[[ $bad -eq 0 ]] && echo "all channels decoded a frame"
exit 0
