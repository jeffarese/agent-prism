#!/bin/sh
# Build agent-prism in release mode and install it as ~/.local/bin/agp
# (or $AGP_INSTALL_DIR/agp).
set -eu
cd "$(dirname "$0")/.."
PATH="$HOME/.cargo/bin:$PATH" cargo build --release -p agent-prism
dest="${AGP_INSTALL_DIR:-$HOME/.local/bin}"
mkdir -p "$dest"
install -m 0755 target/release/agp "$dest/agp"
echo "installed $dest/agp ($("$dest/agp" --version))"
