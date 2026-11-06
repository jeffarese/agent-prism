#!/bin/bash
# Fake `claude` for agent-prism's background-session tests. Never talks to
# Anthropic. It knows `--bg` launches and resumes, `stop <short>` and
# `agents --json --all`, like Claude Code 2.1.316 does:
# - jobs live in <config dir>/jobs/<short>/ (state.json plus the fake's own
#   files sid, name, cwd, flags, pid); a `sleep` stands in for the process;
# - `--bg --resume <id> <flags…> <prompt>` in a profile with no job for the
#   session resumes it under the same id; with a job there (or while it
#   runs) any flag starts a copy under a new id;
# - `--bg --resume <id> <prompt>` wakes the stopped job of this profile.
# Every call is appended to $FAKE_LOG. Test knobs:
# - FAKE_SNAPSHOT=1: `stop` copies agp's record to <job>/record-at-stop.json;
# - FAKE_REVIVE_AFTER_STOP=<n>: the n-th `agents` call after a stop starts
#   the job's process again (someone revived it).
dir="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
jobs="$dir/jobs"
[ -n "$FAKE_LOG" ] && printf 'cfg=%s cwd=%s args=%s\n' "${CLAUDE_CONFIG_DIR-<unset>}" "$PWD" "$*" >> "$FAKE_LOG"

esc() { local s=${1//\\/\\\\}; s=${s//\"/\\\"}; printf '%s' "$s"; }

stamp() { # a distinct ISO time per call
  local n=0
  [ -f "$dir/.seq" ] && n=$(cat "$dir/.seq")
  n=$((n + 1)); echo "$n" > "$dir/.seq"
  printf '%s.%03dZ' "$(date -u +%Y-%m-%dT%H:%M:%S)" $((n % 1000))
}

write_state() { # short state
  local j="$jobs/$1" t
  t=$(stamp)
  [ -f "$j/created" ] || echo "$t" > "$j/created"
  printf '{"state":"%s","detail":null,"needs":null,"sessionId":"%s","cwd":"%s","name":"%s","respawnFlags":%s,"updatedAt":"%s","createdAt":"%s"}\n' \
    "$2" "$(cat "$j/sid")" "$(esc "$(cat "$j/cwd")")" "$(esc "$(cat "$j/name")")" "$(cat "$j/flags")" "$t" "$(cat "$j/created")" > "$j/state.json"
}

# A new session id. Linux distributions don't always ship uuidgen.
new_sid() {
  if command -v uuidgen >/dev/null 2>&1; then uuidgen; else cat /proc/sys/kernel/random/uuid; fi | tr 'A-Z' 'a-z'
}

alive() { [ -f "$jobs/$1/pid" ] && kill -0 "$(cat "$jobs/$1/pid")" 2>/dev/null; }

start_proc() { # short
  sleep 300 </dev/null >/dev/null 2>&1 &
  echo $! > "$jobs/$1/pid"
  write_state "$1" working
}

new_job() { # sid name flags-json
  local short=${1:0:8}
  mkdir -p "$jobs/$short"
  echo "$1" > "$jobs/$short/sid"
  printf '%s' "$2" > "$jobs/$short/name"
  printf '%s' "$PWD" > "$jobs/$short/cwd"
  printf '%s' "$3" > "$jobs/$short/flags"
  start_proc "$short"
  printf '\033[1mbackgrounded\033[0m · \033]8;;x\007%s\033]8;;\007 · %s\n' "$short" "$2"
  echo "note: the session runs in the background"
}

job_of() { # sid → short of this profile's job for it
  local d
  for d in "$jobs"/*/; do
    [ -f "$d/sid" ] && [ "$(cat "$d/sid")" = "$1" ] && basename "$d" && return 0
  done
  return 1
}

case "$1" in
  --version) echo "2.1.316 (fake claude)"; exit 0 ;;
  stop)
    j="$jobs/$2"
    [ -d "$j" ] || { echo "no session $2" >&2; exit 1; }
    if [ -n "$FAKE_SNAPSHOT" ] && [ -n "$AGP_STATE_DIR" ]; then
      cp "$AGP_STATE_DIR/bg/$(cat "$j/sid").json" "$j/record-at-stop.json" 2>/dev/null
    fi
    alive "$2" && kill "$(cat "$j/pid")"
    rm -f "$j/pid"
    [ -n "$FAKE_REVIVE_AFTER_STOP" ] && echo "$FAKE_REVIVE_AFTER_STOP" > "$j/revive"
    write_state "$2" stopped
    echo "stopped $2"
    exit 0 ;;
  agents)
    out="" sep=""
    for d in "$jobs"/*/; do
      [ -f "$d/sid" ] || continue
      s=$(basename "$d")
      if [ -f "$d/revive" ]; then
        n=$(($(cat "$d/revive") - 1))
        if [ "$n" -le 0 ]; then rm -f "$d/revive"; start_proc "$s"; else echo "$n" > "$d/revive"; fi
      fi
      pid=""
      alive "$s" && pid=",\"pid\":$(cat "$d/pid")"
      out="$out$sep{\"id\":\"$s\",\"sessionId\":\"$(cat "$d/sid")\",\"name\":\"$(esc "$(cat "$d/name")")\",\"kind\":\"bg\",\"state\":\"idle\",\"status\":\"idle\"$pid}"
      sep=","
    done
    echo "[$out]"
    exit 0 ;;
esac

# A --bg line: the flags (JSON), --name, --resume and the prompt.
vflags=" --name --settings --model --effort --permission-mode --add-dir --append-system-prompt --agent --mcp-config "
bg="" resume="" name="" flags="" sep="" prompt=""
args=("$@")
i=0
while [ $i -lt ${#args[@]} ]; do
  a="${args[$i]}"
  case "$a" in
    --bg) bg=1 ;;
    --resume) i=$((i + 1)); resume="${args[$i]}" ;;
    -*)
      flags="$flags$sep\"$(esc "$a")\""; sep=","
      if [[ "$vflags" == *" $a "* ]]; then
        i=$((i + 1)); v="${args[$i]}"
        flags="$flags,\"$(esc "$v")\""
        [ "$a" = "--name" ] && name="$v"
      fi ;;
    *) prompt="$a" ;;
  esac
  i=$((i + 1))
done
[ -n "$bg" ] || { echo "fake claude: only --bg lines" >&2; exit 2; }
[ -n "$prompt" ] && [ -n "$FAKE_LOG" ] && printf 'prompt=%s\n' "$prompt" >> "$FAKE_LOG"
flags="[$flags]"

if [ -z "$resume" ]; then
  new_job "$(new_sid)" "$name" "$flags"
  exit 0
fi
short=$(job_of "$resume")
if [ -n "$short" ]; then
  if [ "$flags" != "[]" ] || alive "$short"; then
    # Flags, or the old process still runs: a copy under a new id.
    echo "note: already running in the background, so this started a copy"
    new_job "$(new_sid)" "$(cat "$jobs/$short/name") (copy)" "$(cat "$jobs/$short/flags")"
    exit 0
  fi
  start_proc "$short"
  printf '\033[32mwoke session %s\033[0m with its saved options\n' "$short"
  exit 0
fi
# No job here: resume the same session (the transcript is shared).
[ -z "$name" ] && name="resumed"
new_job "$resume" "$name" "$flags"
