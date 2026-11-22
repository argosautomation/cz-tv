#!/usr/bin/env bash
# Cross-compile the static aarch64 binary. Runs from the app directory
# (pack-deb.sh cd's here). No system cross-gcc is needed: the linker is the Rust
# toolchain's own bundled rust-lld, located under the active sysroot so this
# works in anybody's checkout.
set -euo pipefail
SYSROOT="$(rustc --print sysroot)"
LLD="$(find "$SYSROOT/lib/rustlib" -name rust-lld -type f -print -quit 2>/dev/null || true)"
if [[ -z "$LLD" ]]; then
  echo "error: rust-lld not found under $SYSROOT - is the rustup toolchain complete?" >&2
  exit 1
fi
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER="$LLD"
cargo build --release --target aarch64-unknown-linux-musl
