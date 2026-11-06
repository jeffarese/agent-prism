#!/bin/sh
# Installs agp as ~/.local/bin/agp (or $AGP_INSTALL_DIR/agp). Run it again to
# update.
#
# From a checkout it builds with Cargo:
#   scripts/install.sh
# Anywhere else it downloads the release binary for this machine (macOS,
# Linux or WSL; x86_64 or arm64) and checks its SHA-256:
#   curl -fsSL https://raw.githubusercontent.com/jeffarese/agent-prism/main/scripts/install.sh | sh
#
# --prebuilt downloads even from a checkout. AGP_VERSION=0.2.0 picks a release
# (default: the latest); AGP_DOWNLOAD_URL is where the release files are
# (tests, mirrors).
set -eu
repo=jeffarese/agent-prism
dest="${AGP_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf 'agp: %s\n' "$*"; }
die() { printf 'agp: %s\n' "$*" >&2; exit 1; }

prebuilt=${AGP_PREBUILT:-}
for arg in "$@"; do
  case $arg in
    --prebuilt) prebuilt=1 ;;
    -h | --help) sed -n '2,14p' "$0" 2>/dev/null | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) die "unknown option $arg" ;;
  esac
done

# A checkout: this file sits in <repo>/scripts next to the workspace.
src=
if [ -z "$prebuilt" ] && [ -f "$0" ]; then
  src=$(cd "$(dirname "$0")/.." && pwd)
  [ -f "$src/crates/agent-prism/Cargo.toml" ] || src=
fi

if [ -n "$src" ]; then
  cd "$src"
  PATH="$HOME/.cargo/bin:$PATH"
  command -v cargo >/dev/null 2>&1 ||
    die "needs Rust 1.89 or later (https://rustup.rs), or run with --prebuilt"
  cargo build --release --locked -p agent-prism
  mkdir -p "$dest"
  install -m 0755 target/release/agp "$dest/agp"
else
  command -v curl >/dev/null 2>&1 || die "needs curl"
  case "$(uname -s)" in
    Darwin) os=apple-darwin ;;
    Linux) os=unknown-linux-musl ;;
    *) die "agp runs on macOS and Linux (WSL included), not $(uname -s)" ;;
  esac
  case "$(uname -m)" in
    x86_64 | amd64) arch=x86_64 ;;
    arm64 | aarch64) arch=aarch64 ;;
    *) die "no release build for $(uname -m); build from source instead" ;;
  esac
  asset="agp-$arch-$os"
  if [ -n "${AGP_DOWNLOAD_URL:-}" ]; then
    base=${AGP_DOWNLOAD_URL%/}
  elif [ -n "${AGP_VERSION:-}" ]; then
    base="https://github.com/$repo/releases/download/v${AGP_VERSION#v}"
  else
    base="https://github.com/$repo/releases/latest/download"
  fi
  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  say "downloading $asset from $base"
  curl -fsSL "$base/$asset.tar.gz" -o "$tmp/$asset.tar.gz" ||
    die "no $asset.tar.gz at $base (is there a release yet?)"
  curl -fsSL "$base/SHA256SUMS" -o "$tmp/SHA256SUMS" || die "no SHA256SUMS at $base"
  want=$(awk -v f="$asset.tar.gz" '$2 == f { print $1 }' "$tmp/SHA256SUMS")
  if command -v sha256sum >/dev/null 2>&1; then
    got=$(sha256sum "$tmp/$asset.tar.gz" | cut -d' ' -f1)
  else
    got=$(shasum -a 256 "$tmp/$asset.tar.gz" | cut -d' ' -f1)
  fi
  [ -n "$want" ] && [ "$want" = "$got" ] || die "checksum mismatch for $asset.tar.gz; nothing installed"
  tar -xzf "$tmp/$asset.tar.gz" -C "$tmp"
  "$tmp/$asset/agp" --version >/dev/null 2>&1 || die "the downloaded agp does not run here"
  mkdir -p "$dest"
  # Replace, don't overwrite in place: a running agp keeps its old file.
  cp "$tmp/$asset/agp" "$dest/agp.new"
  chmod 0755 "$dest/agp.new"
  mv -f "$dest/agp.new" "$dest/agp"
fi

say "installed $dest/agp ($("$dest/agp" --version))"
case ":$PATH:" in
  *":$dest:"*) ;;
  *) say "add $dest to your PATH" ;;
esac
command -v claude >/dev/null 2>&1 || command -v codex >/dev/null 2>&1 ||
  say "next: install Claude Code (https://claude.com/claude-code) or the Codex CLI, then run agp"
