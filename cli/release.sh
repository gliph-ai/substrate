#!/bin/sh
# Build the relay for macOS (arm64 + x86_64) and publish it to Substrate's
# bucket, where /install and /downloads/substrate/latest/ serve it.
# GitHub Actions (.github/workflows/release.yml) runs this on every new version.
#   cli/release.sh            build + publish
#   cli/release.sh --build    build only (dist/substrate/)
#   cli/release.sh --publish  publish what dist/substrate/ holds
set -eu
cd "$(dirname "$0")/.."
v=$(awk -F\" '/^version = /{print $2; exit}' Cargo.toml)
out=dist/substrate
if [ "${1:-}" != "--publish" ]; then
  mkdir -p "$out"
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
fi
# A CI token names its own org; on a Mac the stored login needs --org.
org="--org gliph"; [ -n "${PUSHLEFT_TOKEN:-}" ] && org=""
# Versioned binaries first, then latest/ (latest.json, which the installer
# reads to pick the versioned folder, after its binaries), and $v/latest.json
# last as the marker the release workflow checks. A rerun overwrites the same
# paths, so it is safe to retry.
cp_to() { pl $org storage cp "$1" "files:downloads/substrate/$2/$(basename "$1")" --production; }
for f in "$out"/substrate-*; do cp_to "$f" "$v"; done
# ponytail: latest/ binaries stay for installers fetched before the versioned path.
for f in "$out"/substrate-*; do cp_to "$f" latest; done
cp_to "$out/latest.json" latest
cp_to "$out/latest.json" "$v"
echo "published substrate $v"
