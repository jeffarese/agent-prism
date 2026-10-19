#!/bin/sh
# Records the README demo: docs/demo.gif and the images in docs/images/.
# Uses fictional accounts and a fake `claude`. Nothing talks to Anthropic
# or OpenAI, and the real Keychain is not read.
# Needs vhs (https://github.com/charmbracelet/vhs), ffmpeg and uv.
set -eu
cd "$(dirname "$0")/.."
PATH="$HOME/.cargo/bin:$PATH" cargo build --release -p agent-prism
tmp=$(mktemp -d)
trap 'rm -rf "$tmp" demo/out' EXIT
mkdir -p demo/out docs/images

# The GIF: a session hits a limit and moves to another account.
python3 demo/setup.py "$tmp/session" >/dev/null
DEMO_ENV="$tmp/session/env.sh" vhs demo/demo.tape
uv run demo/captions.py demo/out/demo.mp4 "$tmp/session/events.log" docs/demo.gif

# The images: the onboarding, the menu and `agp status`.
python3 demo/setup.py --onboarding "$tmp/onboarding" >/dev/null
DEMO_ENV="$tmp/onboarding/env.sh" vhs demo/screens.tape
# Cut the empty rows under the status table.
ffmpeg -v error -y -i docs/images/status.png -vf crop=iw:400:0:0 demo/out/status.png
mv demo/out/status.png docs/images/status.png
