#!/bin/sh
here="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
sys="$(uname -s)"
case "$sys" in
MINGW* | MSYS* | CYGWIN*) rel="x86_64-pc-windows-msvc/gray-matrix.exe" ;;
Darwin*)
    case "$(uname -m)" in
    arm64) rel="aarch64-apple-darwin/gray-matrix" ;;
    *) rel="x86_64-apple-darwin/gray-matrix" ;;
    esac ;;
*)
    case "$(uname -m)" in
    aarch64 | arm64) rel="aarch64-unknown-linux-musl/gray-matrix" ;;
    *) rel="x86_64-unknown-linux-musl/gray-matrix" ;;
    esac ;;
esac
if [ -x "$here/$rel" ]; then exec "$here/$rel" "$@"; fi
for cand in "$here/target/release/gray-matrix.exe" "$here/target/release/gray-matrix" "$here/target/debug/gray-matrix.exe" "$here/target/debug/gray-matrix"; do
    if [ -x "$cand" ]; then exec "$cand" "$@"; fi
done
echo "gray-matrix binary not found in $here" >&2
exit 1
