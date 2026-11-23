#!/bin/bash
# Verify the read-only iOSBuilder input used by scripts/build-app.sh.
# Usage: verify-iosbuilder-input.sh <iOSBuilder-root>
set -euo pipefail

ROOT=${1:-}
if [ -z "$ROOT" ]; then
  echo "usage: $0 <iOSBuilder-root>" >&2
  exit 2
fi
if [ ! -d "$ROOT" ]; then
  echo "FAIL: iOSBuilder root does not exist: $ROOT" >&2
  exit 2
fi

missing=0
require_dir() {
  local rel=$1
  if [ ! -d "$ROOT/$rel" ]; then
    echo "MISSING directory: $rel" >&2
    missing=1
  fi
}
require_file() {
  local rel=$1
  if [ ! -f "$ROOT/$rel" ]; then
    echo "MISSING file: $rel" >&2
    missing=1
  fi
}

# Prebuilt host dylibs graft the game onto the patched engine.
require_file out-load-ui/host-prebuilt-loadable-ios.dylib
require_file out-load-ui/host-prebuilt-loadable-sim.dylib

# The patched engine is read by the app build and must carry its shader-chain id.
require_dir out-stage/out-engine/engine/dm/tools/nx_extract
require_file out-stage/out-engine/engine/dm/gfx/nvn/dmsh_chain.h

# Native signing and compression inputs.
require_dir out-sign/signing/zsign/src
require_dir out-sign/signing/OpenSSL/Frameworks/OpenSSL.xcframework
require_file out/src/zstd/lib/zstd.h
require_file out/src/zstd/lib/decompress/zstd_ddict.c

# SPIRV-Cross can be prebuilt in out-spvc or built from this source checkout.
if [ ! -f "$ROOT/out-spvc/libspvc-ios.a" ] && [ ! -f "$ROOT/out/src/SPIRV-Cross/spirv_cross_c.h" ]; then
  echo "MISSING SPIRV-Cross: need out-spvc/libspvc-ios.a or out/src/SPIRV-Cross/spirv_cross_c.h" >&2
  missing=1
fi

if [ "$missing" -ne 0 ]; then
  echo "FAIL: the iOSBuilder archive is incomplete; do not run the IPA build yet" >&2
  exit 1
fi

printf 'iOSBuilder input OK: %s\n' "$ROOT"
printf '  host: iOS + simulator\n'
printf '  patched engine: shader chain header + extractor\n'
printf '  signing: zsign + OpenSSL\n'
printf '  compression: zstd\n'
if [ -f "$ROOT/out-spvc/libspvc-ios.a" ]; then
  printf '  SPIRV-Cross: prebuilt iOS archive\n'
else
  printf '  SPIRV-Cross: source available for local build\n'
fi
