#!/usr/bin/env bash
set -euo pipefail
install -D -m 0755 target/aarch64-unknown-linux-musl/release/cz-tv \
  "$STAGE$INSTALL_PREFIX/bin/cz-tv"

# `cz-tv --add <url>` is in the README, in the app's own --help and in every
# answer anybody gets, and until 1.3 none of it worked: APPLaunch's bin
# directory is on nobody's PATH, so a .deb install answered "command not
# found", which reads like being in the wrong directory rather than like a
# packaging mistake. A symlink in the data tree is owned by dpkg, so it is
# removed again on uninstall - a postinst writing into /usr/local/bin would be
# left behind, and /usr/local is the admin's, not a package's.
install -d "$STAGE/usr/bin"
ln -sf ../share/APPLaunch/bin/cz-tv "$STAGE/usr/bin/cz-tv"

# Packs: extra lineups that are installed but NOT in the guide until somebody
# adds one - from the Add channels screen in the app, or `cz-tv --add`.
# See channels/packs/README.md.
for pack in channels/packs/*.m3u; do
  [ -e "$pack" ] || continue
  install -D -m 0644 "$pack" "$STAGE/usr/share/cz-tv/packs/$(basename "$pack")"
done
