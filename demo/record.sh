#!/bin/sh
# Records docs/demo.gif with fictional accounts and a fake `claude`.
# Needs vhs (https://github.com/charmbracelet/vhs) and python3.
# Nothing talks to Anthropic or OpenAI, and the real Keychain is not read.
set -eu
cd "$(dirname "$0")/.."
PATH="$HOME/.cargo/bin:$PATH" cargo build --release -p agent-prism
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
python3 demo/setup.py "$tmp/demo" >/dev/null
DEMO_ENV="$tmp/demo/env.sh" vhs demo/demo.tape
