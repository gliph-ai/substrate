#!/bin/sh
# Substrate relay installer: curl -fsSL __SUBSTRATE_URL__/install | sh
# Downloads the signed-off release for this Mac, checks its SHA-256, and puts
# it in ~/.local/bin. Nothing runs as root.
set -eu
BASE="${SUBSTRATE_URL:-__SUBSTRATE_URL__}"
os=$(uname -s); arch=$(uname -m)
case "$os" in Darwin) os=darwin ;; Linux) os=linux ;; *) echo "substrate: $os is not supported yet" >&2; exit 1 ;; esac
case "$arch" in arm64|aarch64) arch=arm64 ;; x86_64|amd64) arch=x86_64 ;; *) echo "substrate: $arch is not supported yet" >&2; exit 1 ;; esac
name="substrate-$os-$arch"
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
# latest.json names the version; binary and checksum come from that version's
# folder, so they always match even while a release is going out.
v=$(curl -fsSL "$BASE/downloads/substrate/latest/latest.json" | sed -n 's/.*"version" *: *"\([^"]*\)".*/\1/p')
[ -n "$v" ] || { echo "substrate: could not read the latest version." >&2; exit 1; }
echo "Downloading ${name} ${v}..."
curl -fsSL "$BASE/downloads/substrate/$v/$name" -o "$tmp/substrate"
curl -fsSL "$BASE/downloads/substrate/$v/$name.sha256" -o "$tmp/sum"
want=$(cut -d' ' -f1 < "$tmp/sum")
if command -v shasum >/dev/null 2>&1; then got=$(shasum -a 256 "$tmp/substrate" | cut -d' ' -f1); else got=$(sha256sum "$tmp/substrate" | cut -d' ' -f1); fi
[ "$want" = "$got" ] || { echo "substrate: the checksum does not match. Nothing was installed." >&2; exit 1; }
mkdir -p "$HOME/.local/bin"
install -m 0755 "$tmp/substrate" "$HOME/.local/bin/substrate"
[ "$os" = darwin ] && xattr -d com.apple.quarantine "$HOME/.local/bin/substrate" 2>/dev/null || true
echo "Installed ~/.local/bin/substrate"
case ":$PATH:" in *":$HOME/.local/bin:"*) ;; *) echo "Add ~/.local/bin to your PATH." ;; esac
echo
echo "Next:"
echo "  substrate login            # pair this computer"
echo "  substrate daemon install   # keep the relay running; each agent in Herdr gets a project"
