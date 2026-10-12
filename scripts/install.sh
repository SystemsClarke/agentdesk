#!/usr/bin/env sh
# Installs the newest agentdesk release for this machine into ~/.local/bin (or $AGENTDESK_BIN).
# After that, `agentdesk update --apply` keeps it current.   Usage: curl -fsSL <raw url>/scripts/install.sh | sh
set -eu
repo="SystemsClarke/agentdesk"
dest="${AGENTDESK_BIN:-$HOME/.local/bin}"
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)  target=x86_64-unknown-linux-gnu ;;
  Linux-aarch64) target=aarch64-unknown-linux-gnu ;;
  Darwin-x86_64) target=x86_64-apple-darwin ;;
  Darwin-arm64)  target=aarch64-apple-darwin ;;
  *) echo "no build for $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac
url=$(curl -fsSL "https://api.github.com/repos/$repo/releases?per_page=30" |
  grep -o "https://[^\"]*/agentdesk-$target\.tar\.gz" | head -n 1)
[ -n "$url" ] || { echo "no release with a build for $target yet" >&2; exit 1; }
mkdir -p "$dest"
tmp=$(mktemp -d)
curl -fsSL "$url" | tar -xz -C "$tmp"
mv "$tmp/agentdesk" "$dest/agentdesk"
chmod +x "$dest/agentdesk"
rm -rf "$tmp"
echo "installed $("$dest/agentdesk" --version) to $dest/agentdesk"
case ":$PATH:" in *":$dest:"*) ;; *) echo "add $dest to your PATH" ;; esac
echo "coming from the .NET AgentDesk? run: agentdesk migrate     (then agentdesk migrate --apply)"
