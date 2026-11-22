#!/usr/bin/env bash
set -euo pipefail
install -D -m 0755 target/aarch64-unknown-linux-musl/release/cz-tv \
  "$STAGE$INSTALL_PREFIX/bin/cz-tv"

# Packs: extra lineups that are installed but NOT in the guide until somebody
# runs `cz-tv --add` on one. See channels/packs/README.md.
for pack in channels/packs/*.m3u; do
  [ -e "$pack" ] || continue
  install -D -m 0644 "$pack" "$STAGE/usr/share/cz-tv/packs/$(basename "$pack")"
done
