#!/bin/sh
# Build the relay for macOS (arm64 + x86_64) and publish it to Substrate's
# bucket, where /install and /downloads/substrate/latest/ serve it.
#   cli/release.sh            build + publish
#   cli/release.sh --build    build only (dist/substrate/)
set -eu
cd "$(dirname "$0")/.."
v=$(awk -F\" '/^version = /{print $2; exit}' Cargo.toml)
out=dist/substrate; mkdir -p "$out"
for t in aarch64-apple-darwin x86_64-apple-darwin; do
  rustup target add "$t" >/dev/null 2>&1 || true
  cargo build --release -p substrate --target "$t" --locked
  case "$t" in aarch64*) a=arm64 ;; *) a=x86_64 ;; esac
  cp "target/$t/release/substrate" "$out/substrate-darwin-$a"
  codesign -s - -f "$out/substrate-darwin-$a" 2>/dev/null || true
  (cd "$out" && shasum -a 256 "substrate-darwin-$a" > "substrate-darwin-$a.sha256")
done
printf '{"version":"%s"}\n' "$v" > "$out/latest.json"
[ "${1:-}" = "--build" ] && exit 0
for f in "$out"/*; do
  n=$(basename "$f")
  pl --org empire storage cp "$f" "files:downloads/substrate/$v/$n" --production
  pl --org empire storage cp "$f" "files:downloads/substrate/latest/$n" --production
done
echo "published substrate $v"
