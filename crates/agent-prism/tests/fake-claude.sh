#!/bin/bash
# Fake `claude` for agent-prism's tmux end-to-end test. Never talks to Anthropic.
# Prints its args and env, runs agent-prism's hooks from the --settings file it got,
# then reads lines: `limit` → StopFailure rate_limit, `fail` → exit 3,
# `quit` → exit 0.
if [ "$1" = "--version" ]; then echo "9.9.9 (fake claude)"; exit 0; fi

settings="" sid=""
prev=""
for a in "$@"; do
  case "$prev" in
    --settings) settings="$a" ;;
    --session-id|--resume) sid="$a" ;;
  esac
  prev="$a"
done
[ -z "$sid" ] && sid="fake-$$"

echo "FAKE ARGS: $*"
echo "FAKE ENV: CLAUDE_CONFIG_DIR=${CLAUDE_CONFIG_DIR-<unset>} AGP_PROFILE=${AGP_PROFILE-<unset>}"

hook() { # hook <Event> <json>
  [ -f "$settings" ] || return 0
  local cmd
  cmd=$(grep "hook $1\"" "$settings" | head -1 | sed 's/^ *"command": "\(.*\)",*$/\1/')
  [ -n "$cmd" ] && printf '%s' "$2" | eval "$cmd"
}

hook SessionStart "{\"session_id\":\"$sid\",\"source\":\"startup\",\"cwd\":\"$PWD\"}"
echo "FAKE READY session=$sid"
while IFS= read -r line; do
  case "$line" in
    limit) hook StopFailure "{\"session_id\":\"$sid\",\"error\":\"rate_limit\"}"; echo "FAKE sent rate_limit" ;;
    fail) exit 3 ;;
    quit) exit 0 ;;
    *) echo "FAKE got: $line" ;;
  esac
done
