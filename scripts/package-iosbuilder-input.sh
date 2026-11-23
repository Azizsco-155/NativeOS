#!/bin/bash
# Build the private, read-only iOSBuilder archive consumed by build-nativeos.yml.
# Usage: package-iosbuilder-input.sh <iOSBuilder-root> <output.tar.zst>
set -euo pipefail

SRC=${1:-}
OUT=${2:-}
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd -P)
if [ -z "$SRC" ] || [ -z "$OUT" ]; then
  echo "usage: $0 <iOSBuilder-root> <output.tar.zst>" >&2
  exit 2
fi
SRC=$(cd "$SRC" && pwd -P) || { echo "FAIL: source directory does not exist: $1" >&2; exit 2; }
case "$OUT" in /*) ;; *) OUT="$PWD/$OUT" ;; esac
command -v zstd >/dev/null 2>&1 || { echo "FAIL: zstd is required (install it with Homebrew on macOS)" >&2; exit 2; }
mkdir -p "$(dirname "$OUT")"

STAGE=$(mktemp -d)
cleanup() { rm -rf "$STAGE"; }
trap cleanup EXIT

copy_path() {
  local rel=$1
  [ -e "$SRC/$rel" ] || { echo "FAIL: missing iOSBuilder path: $rel" >&2; exit 1; }
  mkdir -p "$STAGE/$(dirname "$rel")"
  cp -R "$SRC/$rel" "$STAGE/$rel"
}

copy_path out-load-ui/host-prebuilt-loadable-ios.dylib
copy_path out-load-ui/host-prebuilt-loadable-sim.dylib
copy_path out-stage/out-engine/engine
copy_path out-sign/signing/zsign
copy_path out-sign/signing/OpenSSL/Frameworks/OpenSSL.xcframework
copy_path out/src/zstd/lib

if [ -d "$SRC/out-spvc" ] && [ -f "$SRC/out-spvc/libspvc-ios.a" ]; then
  copy_path out-spvc
elif [ -d "$SRC/out/src/SPIRV-Cross" ]; then
  copy_path out/src/SPIRV-Cross
else
  echo "FAIL: need out-spvc/libspvc-ios.a or out/src/SPIRV-Cross" >&2
  exit 1
fi

"$SCRIPT_DIR/verify-iosbuilder-input.sh" "$STAGE"
rm -f "$OUT"
tar --use-compress-program=zstd -cf "$OUT" -C "$STAGE" .
sha256=$(shasum -a 256 "$OUT" | awk '{print $1}')
printf 'archive: %s\nsha256: %s\n' "$OUT" "$sha256"
