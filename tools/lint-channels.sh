#!/usr/bin/env bash
# Policy lint for a cz-tv channel list. No network: this is the check that can
# run on every pull request in a couple of seconds and tell a contributor what
# is wrong with their two lines before a human looks.
#
#   tools/lint-channels.sh [file.m3u ...]     (default: channels/*.m3u packs/*)
#
# The rules it enforces are the ones in CONTRIBUTING.md that a machine can see.
# The one that matters most - "does it actually show a picture" - cannot be
# checked without decoding a frame, which is tools/check-channels.sh.
set -uo pipefail

files=("$@")
if [[ ${#files[@]} -eq 0 ]]; then
  mapfile -t files < <(ls channels/*.m3u channels/packs/*.m3u 2>/dev/null)
fi
[[ ${#files[@]} -eq 0 ]] && { echo "lint: no playlists found"; exit 1; }

rc=0
err() { echo "ERROR $1:$2: $3"; rc=1; }
warn() { echo "warn  $1:$2: $3"; }

for f in "${files[@]}"; do
  echo "== $f"
  [[ -s $f ]] || { err "$f" 0 "file is empty"; continue; }
  head -n1 "$f" | grep -q '^#EXTM3U' || err "$f" 1 "must start with #EXTM3U"
  [[ -n $(tail -c1 "$f") ]] && err "$f" "$(wc -l <"$f")" "no newline at end of file"

  n=0; inf=""; infline=0
  declare -A seen_url=() seen_name=()
  while IFS= read -r raw || [[ -n $raw ]]; do
    n=$((n+1))
    line=${raw%$'\r'}
    [[ -z ${line// /} ]] && continue
    if [[ $line == '#EXTINF:'* ]]; then inf=$line; infline=$n; continue; fi
    [[ $line == '#'* ]] && continue

    url=$line
    # --- the URL -------------------------------------------------------------
    [[ $url == https://* ]] || err "$f" "$n" "not https: ${url:0:60}"
    if grep -qiE '(password|passwd|&pwd=|\?pwd=|username=|api_?key=|secret=|token=|auth=)' <<<"$url"; then
      err "$f" "$n" "the URL looks like it carries a credential - that is somebody's login, it cannot ship"
    fi
    host=${url#*://}; host=${host%%/*}; host=${host%%:*}
    if [[ $host =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
      err "$f" "$n" "bare IP host ($host): an unnamed box serving TV is a rebroadcaster, not a publisher"
    fi
    if [[ -n ${seen_url[$url]:-} ]]; then
      err "$f" "$n" "duplicate URL (also line ${seen_url[$url]})"
    fi
    seen_url[$url]=$n

    # --- the #EXTINF ---------------------------------------------------------
    if [[ -z $inf ]]; then
      err "$f" "$n" "a bare URL with no #EXTINF above it - give it a name and a group-title"
      continue
    fi
    if [[ $infline -ne $((n-1)) ]]; then
      warn "$f" "$n" "#EXTINF is not on the line directly above the URL"
    fi
    name=${inf#*,}
    [[ -n ${name// /} ]] || err "$f" "$infline" "no channel name after the comma"
    if [[ ${#name} -gt 28 ]]; then
      warn "$f" "$infline" "name is ${#name} chars; the card's guide shows about 28 (\"$name\")"
    fi
    LC_ALL=C grep -qP '[^\x20-\x7e]' <<<"$name" && \
      warn "$f" "$infline" "non-ASCII in the name; the 7x14 font folds what it can and dots the rest"
    key=${name,,}
    if [[ -n ${seen_name[$key]:-} ]]; then
      err "$f" "$infline" "duplicate channel name \"$name\" (also line ${seen_name[$key]}) - a later one silently replaces the earlier"
    fi
    seen_name[$key]=$infline
    grep -q 'group-title="' <<<"$inf" || \
      err "$f" "$infline" "no group-title=\"...\"; every channel needs a guide group"
    grep -qiE 'drm|widevine|clearkey|playready' <<<"$inf$url" && \
      err "$f" "$n" "DRM: the card has no CDM, this cannot play"
    inf=""
  done < "$f"
  echo "   $((${#seen_url[@]})) channels"
  unset seen_url seen_name
done

[[ $rc -eq 0 ]] && echo "lint: clean"
exit $rc
