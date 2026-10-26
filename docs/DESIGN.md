# agent-prism — design

`agp` starts a normal interactive `claude` session on the Claude account
with the most usable quota. It wraps the session in a PTY and keeps a small
**dock** at the bottom of the terminal that shows the usage of all accounts.
When a limit stops the active account, agent-prism moves the same conversation to
the next account (`--resume`). A hotkey opens a menu and the settings panel.

Platform: macOS (Keychain, `/usr/bin/security`). Language: Rust 2024.

## 1. Concepts

- **Common home** — `~/.claude`. It holds everything that all accounts share.
- **Profile** — one Claude login = one config dir (`~/.claude-<x>`).
  Account-bound files stay in the dir. Shared entries are symlinks into the
  common home.
- **Default install** — the profile whose dir *is* the common home
  (`~/.claude`). Claude is started for it **without** `CLAUDE_CONFIG_DIR`, so
  that Claude keeps using the plain Keychain item `Claude Code-credentials` and
  `~/.claude.json`. Never set `CLAUDE_CONFIG_DIR=~/.claude`.
- **Pool** — the profiles that agent-prism balances across. Profiles out of the pool
  can still be launched with `agp --as <id>`.
- **Session** — one agent-prism process. It owns one `claude` child at a time; a
  switch replaces the child and keeps the conversation (session id).
- **Tool** — Claude (Claude Code, Claude plans) or Codex (Codex CLI, ChatGPT
  plans). Each profile has one tool. `agp` runs Claude, `agp codex`
  runs Codex, and a session moves only between accounts of its tool. The
  Codex side is described in §12; the rest of this document is about Claude
  unless it says otherwise.

A dir with no `oauthAccount` and a custom `ANTHROPIC_BASE_URL` (a local
proxy setup) is never offered as an account.

## 2. Files and paths

| What | Path | Override |
| --- | --- | --- |
| Config | `~/.config/agent-prism/config.toml` | `AGP_CONFIG_DIR` (dir) |
| State root | `~/.local/state/agent-prism/` | `AGP_STATE_DIR` |
| Usage cache | `<state>/usage/<profile>.json` (+ `.lock`, `.samples.jsonl`) | |
| Live sessions | `<state>/sessions/<pid>.json` | |
| Per-run files | `<state>/run/<pid>/` (hook settings, socket `ctl.sock`) | |
| Log | `<state>/agent-prism.log` (rotated at 2 MB) | `AGP_LOG=debug` |
| Adopt journals | `<state>/adopt/<ts>-<profile>.json` | |
| Run journal | `<state>/runs.jsonl` (cut to its newest half at 8 MB) | |

All path defaults are computed in one place (`prism_core::paths`). Library
functions take paths as parameters so that tests use temp dirs. Tests must
never touch the real `~/.claude*`, `~/.config/agent-prism`, or the Keychain.

## 3. Config (`config.toml`)

All keys are optional; the defaults below apply. Writes are atomic
(temp file + rename) and keep unknown keys and comments where practical
(`toml_edit`).

```toml
common_home = "~/.claude"

[claude]
bin = ""                 # empty = first `claude` on PATH that is not agent-prism
args = []                # extra args for every launch, e.g. ["--dangerously-skip-permissions"]

[codex]
common_home = "~/.codex" # the common home of the Codex accounts
bin = ""                 # empty = first `codex` on PATH that is not agent-prism
args = []                # extra args for every `agp codex` launch

[balance]
policy = "smart"         # smart | priority | round-robin | headroom
on_limit = "auto"        # auto | ask | stay | wait
countdown_secs = 5
resume_prompt = "continue"   # first prompt after a limit switch when Claude has work (§9); "" = none
soft_5h = 90             # new sessions avoid an account at/over this 5h %
hard = 98                # at/over this % a window counts as limited

[usage]
poll_active_secs = 60
poll_idle_secs = 180
token_refresh = "lazy"   # lazy | direct  (see §6.3; "direct" is not built yet)

[dock]
mode = "auto"            # auto | full | compact | hidden
show = "pool"            # pool | all
projections = true
icons = "nerd"           # nerd (needs a Nerd Font) | unicode

[keys]
menu = "ctrl+\\"

[backup]
target = "~/.local/state/agent-prism/backups"   # or "ssh://backup-host/backups/agent-prism"

[[profile]]
id = "acme-al"
label = "Acme Al"
dir = "~/.claude-work"
tool = "claude"          # claude (default, not written) | codex
pool = true
weight = 1.0
reserve_7d = 0           # % of the weekly limit agent-prism leaves unused (someone else uses it)
color = "cyan"           # optional; else a stable color from the palette
args = []                # extra claude args for this profile
env = {}                 # extra env for this profile

[share]
extra_shared = []        # entry names to add to the shared set
extra_local = []         # entry names to force local
sync_trust = true        # give every Claude account the folder trust of the others (§4.7)
```

`dir` is stored with `~`; it is expanded to an absolute path with no trailing
slash before any use. That absolute string is the exact value of
`CLAUDE_CONFIG_DIR` and the input of the Keychain hash (§6.1). Never change
its spelling.

## 4. Profiles and sharing (`prism-core`)

### 4.1 Identity

`.claude.json` is `<dir>/.claude.json`, or `~/.claude.json` for the default
install. Read `.oauthAccount.{displayName,emailAddress,organizationName}`.
Plan data comes from the Keychain blob (`subscriptionType`, `rateLimitTier`,
e.g. `team` / `default_claude_max_5x`). A dir with no `oauthAccount` and no
credentials is "not logged in". A dir whose `settings.json` sets
`ANTHROPIC_BASE_URL` or whose launch uses an API key is "not an Anthropic
login" and is never offered.

### 4.2 Share map

Every top-level entry of a profile dir is in one class:

**shared** (symlink → `<common>/<name>`):
`CLAUDE.md`, any top-level `*.md`, `settings.json`, `settings.local.json`,
`keybindings.json`, `agents`, `commands`, `skills`, `hooks`, `output-styles`,
`rules`, `plugins`, `statusline`, `statusline*.sh`, `superpowers`, `dev-mods`,
`projects`, `file-history`, `todos`, `tasks`, `teams`, `plans`,
`history.jsonl`, `paste-cache`, `shell-snapshots`, `session-env`, `uploads`,
`downloads`.

**local** (must be a real file/dir of the profile, never a link into common):
`.claude.json`, `.credentials.json`, `.oauth_refresh.lock*`,
`policy-limits.json*`, `remote-settings.json*`, `daemon`, `daemon.*`,
`sessions`, `jobs`, `cache`, `.cache`, `backups`, `stats-cache.json`,
`mcp-needs-auth-cache.json`, `statsig`, `telemetry`, `.last-*`, `chrome`,
`ide`.

**unmanaged** — everything else (`scheduled`, `state`, `feedback`, `*.bak*`,
`*.premerge`, unknown names). agent-prism never changes them; `doctor` lists them.

`share.extra_shared` / `share.extra_local` move names between classes.

Why `projects` is shared: transcripts must be visible to every account so
that `claude --resume <id>` works after a switch; auto-memory also lives
there. Why `daemon`, `sessions`, `.cache` are local: they hold a background
daemon with keys, live-process records, and the status-line cache, which are
tied to one account. (The old clausona setup linked them into `~/.claude`;
adopt fixes that.)

### 4.3 Adopt (`agp adopt <profile> [--apply]`)

Inventory → plan → (backup) → apply → journal. Without `--apply` it prints
the plan and changes nothing. The plan is a list of actions:

- `Link(name)` — shared entry missing: create symlink.
- `Merge(name)` — shared entry is a real file/dir in the profile: merge it
  into common, then replace it with the symlink.
- `Relink(name)` — shared entry is a symlink to somewhere else: replace.
- `Localize(name)` — local entry is a symlink (into common or elsewhere):
  replace with an empty dir (if the target is a dir) or remove it (file;
  Claude recreates it).
- `Skip(name, reason)` — already correct, or unmanaged.

Merge rules (same filesystem → `rename`):

- dir: walk the profile tree; file absent in common → move; identical bytes
  → drop the profile copy; different → keep common, move the profile copy to
  `<common>/.agent-prism-conflicts/<profile>/<relpath>`. Then remove the (now empty)
  profile dir and create the symlink. A file that appears in common after
  the plan is never replaced (no-clobber move; identical → drop, else
  conflict).
- auto-memory (`projects/<slug>/memory/`): same rule, `MEMORY.md` included.
  Adopt never edits a live memory file; a differing copy goes to
  `.agent-prism-conflicts` and the dry run, the apply output and `doctor` list it
  for a manual reconcile. `.git`, `.gitignore` and `.memory-store` of a
  memory dir (the memory store's repo) are never merged: a profile copy
  goes to `.agent-prism-conflicts`.
- `history.jsonl`: union of lines from both, dedup exact lines, stable sort
  by the `timestamp` field (lines without it keep their order at the end),
  written atomically to common.
- `settings.json` (or other single file) that differs from common: do not
  overwrite common. Save the JSON keys whose values differ as an overlay at
  `~/.config/agent-prism/profiles/<id>/settings.json` (agent-prism passes it with
  `--settings` on launch, §7.4) and report it. Non-JSON file → conflict copy.

Safety:

- Refuse to apply while a `claude` process uses the profile (§4.5) unless
  `--force`.
- Backup first (§4.4) unless `--no-backup`. Abort on backup failure.
- Apply is idempotent: a second run is all `Skip`.
- Journal every action with source/destination to `<state>/adopt/`.
- Never follow a symlink out of the profile while walking.

### 4.4 Backup

`tar -czf` of the profile dir (`--no-mac-metadata` to skip `._` files) plus
`<common>/history.jsonl`. Target `ssh://host/path` streams over
`ssh -o BatchMode=yes host 'mkdir -p path && cat > path/<name>.tar.gz'`, then
checks remote size > 0 with `ssh host stat`. A local target writes the file.
Name: `<YYYY-MM-DD>-<profile>-<HHMMSS>.tar.gz`.

### 4.5 Running-Claude detection

`ps -Ao pid=,comm=` for `claude` processes, then `ps -Eww -o command= -p <pid>`
(own processes only) and look for `CLAUDE_CONFIG_DIR=<dir>`. A claude with no
`CLAUDE_CONFIG_DIR` belongs to the default install. Best effort; report
"unknown" when env is not readable.

### 4.6 Add account (`agp add [id]`)

1. Ask id (slug) and label. Dir = `~/.claude-<id>`. Refuse if it exists and
   is not empty (offer "adopt instead").
2. Create the dir (0700), create all shared symlinks, create empty local dirs.
3. Leave the TUI (if any), print a clear header, and run
   `CLAUDE_CONFIG_DIR=<dir> claude auth login` attached to the terminal.
   Return to the TUI after it exits.
4. Read identity (§4.1). Show it. Add `[[profile]]` with `pool = true`.

### 4.7 Folder trust

Claude keeps "trust this folder" in the account's `.claude.json` (§4.1), at
`projects["<abs path>"].hasTrustDialogAccepted`; no other field marks trust.
Claude also counts a trusted parent dir; inside a git repo it looks only up
to the repo root (Claude 2.1.310). Codex keeps trust in `config.toml`
(`[projects."<path>"] trust_level`), which all Codex accounts share (§12), so
it needs nothing.

Before Claude starts on an account (first start and every switch; not print
mode, which shows no dialog), `prism_core::trust::sync` gives that account
the trust of every folder that another Claude account trusts (the union).
It only adds trust; it never removes it.

- Existing entry: only `hasTrustDialogAccepted` becomes `true`; its other
  fields stay. No entry: Claude's own default entry (`allowedTools: []`,
  `mcpServers: {}` … the entry Claude writes when the dialog is accepted in a
  new folder) with trust on, at the end of `projects`. Other accounts'
  `allowedTools`, MCP approvals and stats are not copied.
- The write keeps the rest of the file as it is: key order
  (`serde_json` `preserve_order`), numbers (`float_roundtrip`) and Claude's
  2-space format. Temp file + rename in the same dir, through a symlinked
  file. No write when nothing changes.
- A missing target file (a new login) is left to Claude. A file that is
  unreadable or not JSON is skipped with a warning in the log; the launch
  never fails because of the sync.
- Race: a Claude already running on that account can write the file at the
  same time. agent-prism takes Claude's own lock (the dir
  `<.claude.json>.lock`, proper-lockfile; a lock older than 10 s is stale) for
  its read-modify-write, and Claude re-reads the file under that lock before
  it writes, so neither side loses the other's change. If the lock is held for
  more than 1 s, the sync skips this start. A Claude version that writes
  without the lock could still overwrite the change; the next start adds it
  again.

`share.sync_trust = false` turns it off. `doctor` shows, per Claude account,
how many folders it trusts and how many only other accounts trust.

## 5. Shared data model (`prism_core::model`)

The dock, the balancer and the settings UI use these types (already in the
skeleton). Times are unix seconds (`i64`). Percentages are `f64` 0–100.

## 6. Usage (`prism-usage`)

### 6.1 Token

Keychain service: default install → `Claude Code-credentials`; other
profiles → `Claude Code-credentials-<first 8 hex of sha256(dir string)>`.
Example: `/Users/me/.claude-work` → `1e91dd84`. Read with
`/usr/bin/security find-generic-password -s <service> -w`. The value is JSON:
`{"claudeAiOauth":{"accessToken","refreshToken","expiresAt"(ms),"scopes",
"subscriptionType","rateLimitTier"}}`. Fallback: `<dir>/.credentials.json`
(often stale).

**Secrets rule:** never print, log, cache, or put a token in an error
message. The token type has a redacting `Debug`. Only `expiresAt`,
`subscriptionType`, `rateLimitTier` may be stored or shown.

### 6.2 API

`GET https://api.anthropic.com/api/oauth/usage` with
`Authorization: Bearer <access>`, `anthropic-beta: oauth-2025-04-20`,
`User-Agent: agent-prism/<ver>`; timeout 10 s. Parse permissively (every field
optional, unknown fields ignored):

- `five_hour`, `seven_day` (and any other top-level `{utilization,resets_at}`
  objects such as `seven_day_opus`): `utilization` is a percent;
  `resets_at` is ISO-8601 with microseconds and an offset
  (`2026-10-15T09:00:00.123456+00:00`).
- `limits[]`: `{kind, percent, resets_at, scope:{model:{display_name}}}`;
  `kind == "weekly_scoped"` → a `ScopedWindow` labelled with the model
  display name (e.g. "Fable").
- `extra_usage.is_enabled`.

401 → token expired/invalid; 429 → back off (`Retry-After`, else 5 min).

### 6.3 Expired tokens

Claude refreshes its own token only while it runs for that profile. An idle
profile's token expires after hours. `token_refresh = "lazy"` (built now):
keep the last snapshot, mark it stale, and apply inference (§6.5). `direct`
(OAuth refresh by agent-prism + Keychain write-back) is **not** built until we
verify Claude's refresh lock and Keychain format — leave a clear extension
point and do not write to the Keychain.

Verified 2026-10-15: `claude auth status` does not refresh an expired token
(it reads local data only). Claude refreshes through
`https://platform.claude.com/v1/oauth/token` under its own lock
(`oauth_refresh_lock_*`). The user chose `lazy` as the behavior.

### 6.4 Cache and polling

Each agent-prism process polls in a background thread. Before a fetch it reads
`<state>/usage/<id>.json`; if younger than the poll interval it uses it.
Else it takes `fs4` try-lock on `<id>.lock` (skip if another agent-prism holds it),
fetches, writes atomically, and appends `{ts,u5h,u7d}` to `.samples.jsonl`
(keep the last 7 days). Active profile: `poll_active_secs`; others:
`poll_idle_secs`. A "refresh now" request ignores the age.

### 6.5 Inference and projection

- If a window's `resets_at` is in the past, its utilization is 0 and its
  reset is unknown (`inferred = true`).
- Projection: from samples in the last 60 min (5h window) or 6 h (7d window),
  weighted toward recent samples, project the utilization at reset. Needs
  ≥ 10 min of span, else none.

### 6.6 Balancer

Input: `AccountView`s for pool profiles, the model in use (for scoped
windows), sessions per profile from the live-session registry.

- **Limited**: `u5h >= hard`, or `u7d >= 100 - reserve_7d`, or a scoped
  window for the session's model `>= hard`. Limited accounts are never
  picked.
- **smart** score, highest wins:
  ```
  slack7 = max(0, 100 - reserve_7d - u7d) / 100
  days   = max(0.25, hours_to_7d_reset / 24)       (unknown reset → 7)
  score  = cap * weight * ( 0.45 * (1 - u5h/100)
                          + 0.35 * min(1, slack7 / days)
                          + 0.20 * (1 - u7d/100) )
         - 0.10 * sessions_elsewhere
         - (u5h >= soft_5h ? 0.50 : 0)
         - (usage unknown ? 0.15 : 0)
  cap = 2.0 for "20x" tiers, 1.0 for "5x" and team, 0.4 for pro, else 1.0
  ```
  Tie → least recently used.
- **priority**: first eligible in config order (fill one, then spill).
- **round-robin**: least recently used eligible.
- **headroom**: max of `min(100-u5h, 100-reserve-u7d)`.
- Every pick has a one-line reason, e.g. "weekly resets in 21h with 28%
  unused", "most 5h headroom", "the only account not limited".
- All limited → no pick; report the earliest reset.
- Mid-session agent-prism never switches for balance alone — only on a limit or
  a manual switch (prompt cache is per account; a switch re-reads context).

## 7. The wrapper (`prism-term`)

### 7.1 Model: passthrough + reserved rows + shadow emulator

Claude runs in a PTY sized `(rows - dock_h) × cols`. Child output goes
**unchanged** to the real terminal (so kitty keyboard, OSC 8 links, OSC 52,
images, notifications, queries and replies all work natively), except a
small streaming filter. The same filtered bytes feed a `vt100` parser of the
child size (the shadow). The dock is drawn into the bottom `dock_h` rows.
The user runs Claude with `"tui": "fullscreen"` (alt screen), which makes
this reliable; main-screen mode is best effort.

### 7.2 Output filter (stateful, chunk-boundary safe)

Let `N = child_rows`.

- `CSI Pt;Pb r` (DECSTBM): clamp `Pb` to `N`; `CSI r` → `CSI 1;N r`.
- `CSI Pr;Pc H`, `CSI Pr;Pc f`, `CSI Pr d`: clamp row to `N`.
- After a chunk that contains RIS (`ESC c`), DECSTR (`CSI ! p`), or an
  alt-screen toggle (`CSI ? 1049/1047/47 h|l`), inject `CSI 1;N r` and then
  restore the cursor from the shadow.
- Mark the dock damaged after: any `CSI … J` (ED), RIS, DECSTR, alt-screen
  toggle, `CSI ? 3 h|l`.
- Never inject in the middle of an escape sequence or of a UTF-8
  character (a PTY read can split `─`, 3 bytes): inject only when the parser
  is in ground state with no partial character; otherwise defer.
- Track synchronized output (`CSI ? 2026 h/l`): if the child is inside a
  sync block, defer the dock draw until it closes. Wrap agent-prism's own draws in
  `CSI ? 2026 h` … `CSI ? 2026 l`.

### 7.3 Drawing the dock

Render the dock with ratatui into an off-screen `Buffer` the size of the dock
rect, diff against the last drawn buffer, and emit the changed cells as
escape sequences with absolute positions (rows `N+1..rows`). Then restore the
child state from the shadow: cursor position (CUP), SGR attributes,
cursor visibility. Do **not** use DECSC/DECRC (`ESC 7/8`) — that clobbers the
child's own saved cursor. Full redraw after damage, resize, or a mode change.
Throttle draws to ≤ 20/s; draw within one frame after output goes quiet
(16 ms) when the dock is dirty.

### 7.4 Child process

- Spawn with `openpty` + `setsid` + `TIOCSCTTY` (or `portable-pty` if it
  exposes what we need): we need the pid, `waitpid` exit status, resize
  (`TIOCSWINSZ`), and the process group for signals.
- Env: inherit; for non-default profiles set `CLAUDE_CONFIG_DIR=<dir>`; for
  the default install **remove** `CLAUDE_CONFIG_DIR`. Add `AGP=1`,
  `AGP_SOCK=<run>/ctl.sock`, `AGP_PROFILE=<id>`, `AGP_LABEL=<label>`, profile `env`.
- Args: `config.claude.args` + profile `args` + the user's passthrough args +
  agent-prism additions: `--settings <run>/settings.json` (hooks, §8; merged with a
  per-profile overlay and with a user `--settings` if given) and, on a fresh
  start without a user session flag, `--session-id <uuid v4>`.
  The settings (wrapper and print mode) also get `autoMemoryDirectory` =
  the real path behind the shared `projects/` symlink plus Claude's project
  key, because Claude prompts for (or in print mode denies) memory writes
  through the symlink to `~/.claude`. Only for non-default profiles, when no
  memory env var or `autoMemoryDirectory` setting exists and the key is sure
  (ASCII, at most 200 chars, dir exists or cwd is the project root).
- Session flags of the user (`-r/--resume`, `-c/--continue`, `--session-id`,
  `--fork-session`, `--from-pr`): agent-prism does not add `--session-id`; it
  learns the id from the `SessionStart` hook.
- Print mode (`-p/--print` before any `--`): no wrapper. `agp -p ...`
  works like `claude -p ...`: same args, stdin, stdout and exit code, plus
  the account env and `--settings`. agent-prism never prompts, never runs
  onboarding and never fails the run itself. No config or no accounts: one
  stderr note, then plain claude. All accounts limited: one stderr note, then
  the earliest-reset account. Overlay and user `--settings` are merged into
  one JSON value (user wins); if that fails the user value goes through. Same
  inside an agent-prism session (nested).
- Print mode with `--on-limit` (§11a): agent-prism stays the parent
  (`supervise.rs`) instead of exec'ing the tool.
- Exit: when the child exits (not by an agent-prism switch), restore the terminal
  (`CSI r`, show cursor, clear the dock rows, leave raw mode) and exit with
  the child's code.
- Job control: if the child stops (Ctrl+Z inside Claude), restore the
  terminal and stop agent-prism (`SIGTSTP` to self); on `SIGCONT` re-enter raw
  mode, resize, redraw, `SIGCONT` the child group.
- `SIGWINCH`: recompute layout, resize the PTY, re-inject `CSI 1;N r`, clear
  and redraw the dock. `SIGTERM`/`SIGHUP`: forward to the child, then clean up.
- At start in main-screen mode, print `dock_h` newlines first so the dock
  does not cover shell output.

### 7.5 Input

Raw mode on stdin. Bytes go to the child unchanged, with these exceptions:

- The menu key (default Ctrl+\\): legacy `0x1c`, kitty `CSI 92;5u` (also
  with event-type/extra fields), xterm `CSI 27;5;92~`. It opens the menu.
  Pressing it again inside the menu sends a literal Ctrl+\\ to Claude.
- SGR mouse reports (`CSI < b;x;y M|m`) with `y > N`: handled by the dock,
  not forwarded.
- Terminal replies (DA, CPR, OSC 10/11, DECRPM, kitty `CSI ? flags u`) are
  forwarded untouched.
- A read that ends in an incomplete `ESC [`/`ESC` sequence: hold it ≤ 20 ms
  for the rest, then flush as is (a lone Esc must reach Claude fast).

While a panel has focus, all keys go to the panel. On open, agent-prism pushes
kitty keyboard flags 0 (`CSI > 0 u`) and pops on close (`CSI < u`) so that
the panel sees legacy keys; the parser still accepts `CSI u` forms. Bracketed
paste goes to the focused text field.

### 7.6 Layout

- `full`: 1 separator row + 1 row per shown account + 1 status row.
- `compact`: 1 row.
- `hidden`: 0 rows (the menu key still works).
- `auto`: `full` if `rows >= 30` and `full height <= rows / 5`, else `compact`.
- Menu/panel open: dock height = `max(12, 60% of rows)`; the child shrinks
  and repaints itself (it gets SIGWINCH). Closing restores the height.

### 7.7 Dock look (full, width 120)

```
──────────────────────────────────────────────────────────── agent-prism · smart ─
 ● Acme Al   team  5h ███████▌░░░░░░░  48% ↻2h14m  7d ████▊░░░░░░░░░░ 31% ↻3d4h   Fable 12%
 ○ Acme Bea  team  5h ▏░░░░░░░░░░░░░░   1%         7d ██████████▉░░░░ 72% ↻21h    Fable  —    next ▸
 ⌃\ menu · session 3f2a91c0 · on Acme Al since 10:42 · usage 40s ago
```

- `●` this session, `◐` used by another agent-prism, `○` idle, `✕` limited
  (red, "limit · resets 1h12m"), `⚠` token expired (dim, "stale 19h").
- Bars use eighth blocks; color by level: < 60 green, < 85 yellow, ≥ 85 red;
  the track is dim. Inferred values are dim with `~`. Projection, when on
  and wide enough: `48%→71%`.
- The status row shows messages and the switch countdown:
  `Acme Al hit the 5h limit · moving to Acme Bea in 4s · ⏎ now · esc stay`.
- Narrow terminals drop columns in this order: projection, scoped, reset
  times, tier, bars (keep percentages).
- Compact: ` ● Acme Al 5h 48% 7d 31% │ ○ Acme Bea 5h 1% 7d 72% ▸ │ ⌃\ `.
- Colors follow the terminal palette (ANSI 16 + bold/dim), so light and dark
  themes both work.

## 8. Hooks and session tracking

agent-prism writes `<run>/settings.json` with hooks for `SessionStart`,
`UserPromptSubmit`, `Stop`, `StopFailure`, `SessionEnd`; each runs
`<abs agp path> hook <Event>`. Hooks from `--settings` are **added** to the
user's hooks (verified 2026-10-15 with Claude 2.1.300).

`agp hook <Event>`: if `AGP_SOCK` is unset, exit 0 at once. Else read
stdin (≤ 1 MB), connect to the socket (200 ms timeout), send one line
`{"event":"<Event>","payload":<stdin JSON>}`, exit 0. Never print. Never
fail the hook.

Payload facts (verified): all events carry `session_id`, `transcript_path`,
`cwd`; `SessionStart.source` is `startup|resume|clear|compact`;
`StopFailure.error` is one of `rate_limit, overloaded,
authentication_failed, billing_error, invalid_request, model_not_found,
server_error, max_output_tokens, cloud_credential_error, unknown`, plus
`error_details` and `last_assistant_message`.

State: `session_id` and `transcript_path` (latest `SessionStart`), `busy`
(after `UserPromptSubmit` until `Stop`/`StopFailure`), `last_failure`.
`busy` misses turns that no user prompt starts: a teammate message, a task
notification, or a `/goal` Stop hook that keeps Claude working.

## 9. Switching

Triggers: `StopFailure` with `error == rate_limit` (refresh usage of the
active account at once); a poll that shows the active account limited; the
user ("Switch account now" in the menu, pick a target or "best").

1. Target = balancer pick excluding the current account (or the user's).
   None → status message with the earliest reset; stop.
2. `on_limit`: `auto` → countdown (`countdown_secs`; ⏎ now, Esc cancel);
   `ask` → prompt in the status row; `stay` → message only; `wait` → no
   target: stay on the account until its reset + 30 s (unknown reset: check
   again in 5 min), with "waiting for the reset · going on in …" in the
   status row; the menu still offers a manual switch. At the deadline, if
   the account is still limited with a later reset, wait again; else go on:
   with work to carry on (same rule as step 5) restart the tool on the same
   account with `--resume` and the resume prompt ("agent-prism waited for
   the reset on the same account"), otherwise only a message. A reset
   further away than `--max-wait` → message only. Manual switch → no
   countdown.
3. If `busy` and the trigger is manual, confirm first ("Claude is working").
4. SIGTERM to the child's process group; wait ≤ 3 s; SIGKILL.
5. Spawn on the target with the same cwd and args, minus the user's session
   flags and positional prompt, plus `--resume <session_id>` and, for a limit
   trigger with `resume_prompt` set, a first prompt as a positional argument
   when Claude has work to carry on: a `StopFailure` stopped the turn, or the
   end of the transcript shows an open turn (an unanswered prompt, teammate
   message, notification or tool result, or a running tool) or an active
   `/goal` (the latest `goal_status` attachment is neither met nor failed).
   Claude Code restores the goal on `--resume`, but its Stop hook only acts
   when a turn ends; without a prompt the resumed session waits for input.
   The prompt is a one-line note (account, limit, and that background tasks
   and subagents of the old process have stopped), a blank line, then
   `resume_prompt`. The transcript scan reads at most the last 64 MB.
6. If the new child exits non-zero within 5 s, show the error and offer
   "go back".
7. Requires shared `projects/` in both profiles; else the switch item is
   disabled with "run agp adopt <id>".

Live-session registry `<state>/sessions/<pid>.json`
(`{pid, profile, session_id, cwd, started_at}`); dead pids are ignored and
removed. It gives "used by another agent-prism" and the balancer's
`sessions_elsewhere`.

## 10. Menu, settings, onboarding (`agp` bin)

Menu (opens in the dock):

```
─ agent-prism ───────────────────────────────────────────────────────
  w  Switch account now ▸      d  Dock: full · compact · hidden
  s  Settings                  r  Refresh usage
  a  Accounts                  l  Log
  ?  Help                      ⌃\ Send ⌃\ to Claude     esc Close
```

Settings panel (in the dock, or full screen via `agp settings`), tabs:
**Accounts** (list, status, pool toggle, weight, reserve, color, label,
add, adopt, login again, remove-from-agent-prism), **Balancing** (policy with a
live "next pick" preview and its reason, on_limit, countdown, resume prompt,
soft/hard), **Dock** (mode, show, projections, icons, live preview), **Sharing**
(per profile: health, plan preview, apply adopt), **Keys**, **About**
(paths, version, doctor summary). Every change saves at once (atomic) and
applies live; running agent-prism sessions reload the config when its mtime changes.

Onboarding (first `agp` with no config, or `agp setup`), full screen,
left step rail + content + key hints:

1. Welcome — what agent-prism does, in 3 lines.
2. Accounts — detected profiles (`~/.claude*` + known dirs): name, email,
   org, plan, login state, share health; checkbox "in pool"; "+ Add new
   account". Proxy dirs are not listed.
3. Sharing — per selected profile: what becomes shared, merged, localized;
   conflicts; backup target. Apply / Skip.
4. Balancing — policy and limit behavior, each with a one-line explanation.
5. Dock — live preview with real usage; mode.
6. Done — summary, `agp` to start, the menu key, `agp add`.

## 11. CLI

```
agp [--as <id>] [--policy <p>] [--dock <mode>] [claude args...]
agp codex [--as <id>] [--policy <p>] [--dock <mode>] [codex args...]
agp status [--json] [--watch]
agp pick [--json] [--codex]
agp add [--codex | --claude] [id]
agp settings | setup | doctor
agp dashboard [--port <n>] [--no-open]
agp skill [install]             print or install the agp skill (§11a)
agp adopt <id|dir> [--apply] [--no-backup] [--force]
agp exec <id> -- <cmd...>       run a command with that profile's env
agp hook <Event>                internal
```

agent-prism launch flags count only as a leading prefix. The first other arg and
everything after it go to claude unchanged; a leading `--` ends agent-prism's
flags. Lines with `-p/--print` always go to claude, even `agp status -p`
(`exec` and `hook` excepted). agent-prism adds its own args before any user `--`.
Inside an agent-prism session (`AGP_SOCK` set and its socket accepts a connection),
interactive `agp` warns and execs plain claude. A dead socket is an `AGP_SOCK` left in a
multiplexer (herdr, tmux) started from a session that has since exited, so `agp` starts a
full session there.

## 11a. Per-run options and supervised print mode

Launch flags (a leading prefix, like `--as`): `--on-limit
<switch|ask|stay|wait|fail>`, `--max-wait <dur>`, `--accounts <ids>`,
`--exclude <ids>`, `--max-usage <pct>`. `RunOptions::apply` changes the
loaded config in memory for this run only: `--accounts` sets `pool` to
membership (out-of-pool accounts can be listed), `--exclude` clears `pool`,
`--max-usage` lowers `balance.hard` and `soft_5h` to the percent, and
`--on-limit` (all but `fail`) sets `balance.on_limit`. A session applies
them again after every config reload. Unknown ids are an error. `fail` in
an interactive session is an error.

Print mode with `--on-limit` (`agp -p`, `agp codex exec`, any direct codex
line): agent-prism spawns the tool as a child instead of exec. Claude gets
the usual print settings plus the hooks (§8) on a socket of the run (a
per-user temp path when `<run>/ctl.sock` is longer than 103 bytes).

1. Pick: `--as`, else the balancer. Nothing usable (or the `--as` account
   is limited): `wait` → sleep until the earliest reset + 30 s, waking every
   5 min to see if the account is free; `fail` → exit 75; else run on the
   account that resets first, as plain print mode does.
2. Run the attempt. stdin is inherited by the first attempt only; stdout is
   buffered unless it streams (`--output-format stream-json`, codex
   `--json`); stderr passes through. Signals go to the child.
3. Limit: a `StopFailure` with `rate_limit`, or a non-zero exit while the
   account (usage refreshed) is limited. Codex has no hooks: only the
   second test. Else write the buffered stdout and exit with the code.
4. On a limit: `stay` → exit with the code; `fail` → 75; `wait` → sleep
   until the account's reset (+ 30 s), then resume on it; `switch`/`ask` →
   resume on the best other account (none → exit with the code). Waits
   together are capped by `--max-wait` (over it → 75). At most 20
   recoveries per run. Resume: Claude `--resume <session id from
   SessionStart>` with every positional removed from the user args and the
   resume prompt (§9, "this run moved to account …") as the prompt; Codex
   (`exec` lines only) `exec <user flags> resume <thread id> <prompt>`, the
   thread being the newest one in the work dir since the attempt started
   (any `thread_source`). No session id → exit with the code.

## 11b. Run journal

`<state>/runs.jsonl` (`prism_core::journal`): one JSON line per event of a
run, for `agp dashboard`: `ts`, `run` (`<pid>-<start ts>`), `ev`
(`start | limit | wait | switch | resume | end`), `tool`, `mode` (`tui` =
session, `exec` = print mode that replaced itself, so no `end`, `print` =
supervised print mode), `profile`, and when they apply `from`, `limit`
(`5h | 7d | scoped`), `until`, `reason`, `session`, `cwd`, `exit`, `secs`.
A line is one `write` in append mode, so processes don't interleave. Errors
are logged and never fail a run.

## 11c. Dashboard (`agp dashboard`)

Code: `crates/agent-prism/src/dashboard.rs` and `dashboard/` (the page is
`index.html`, `app.css`, `app.js`, embedded with `include_str!`).

**Server.** `std::net::TcpListener` on `127.0.0.1:<port>` (0 = any free
port), a thread per connection (at most 32), one request per connection.
Only `GET`. The `Host` header must be `127.0.0.1:<port>` or
`localhost:<port>` (DNS rebinding), and a request with `Sec-Fetch-Site:
cross-site | same-site` is refused. No CORS headers. The page has a strict
CSP (`default-src 'none'`, scripts and styles from the server only, so no
inline script or style; the JS sets styles through CSSOM). SIGINT and
SIGTERM set a flag; the accept loop polls it and stops.

**Endpoints.** `/api/state`: the same calls as `agp status --json`
(`account_inputs`, `load_usage`, `projections`, `build_views`, the pick
per tool), mapped to a dashboard shape: identity, plan and tier, token
state as a word (`valid | expired | missing | unreadable`; the text of a
read error is never sent), windows with projections, scoped windows,
limited, pick and reason, live sessions, the color slot.
`/api/history?range=24h|7d`: the usage samples per account in buckets of
5 min (24 h) or 30 min (7 d), with the peak of each window per bucket.
`/api/activity?range=…&tool=claude|codex`: runs rebuilt from the journal
(3 days of slack before the range so that long runs keep their start) and
summarized for the range.

**Runs.** The events of a run id, in time order, give segments: the
account from the start, a new segment at each switch, a waiting segment
from `wait` to `resume`. A run without `end` is live when the registry has
its pid (with `started_at` within 2 min of the start; it then ends now);
else it ends at its last event. Run time counts only the non-waiting part
of segments inside the range.

**Tokens.** Only the sessions that the journal names are read. Claude:
`<common>/projects/<slug>/<session>.jsonl` plus `<session>/subagents/*.jsonl`
(slug = cwd with every non-alphanumeric char as `-`; else every project
dir is checked for the file). Assistant lines with `message.usage`;
streamed chunks repeat `message.id`, and the last one wins. Codex: the
rollout file of the thread under `<codex common>/sessions/YYYY/MM/DD/`
(an index of all rollout names, rebuilt at most every 5 min); a
`token_count` event counts its `last_token_usage` when
`total_token_usage.total_tokens` grew (repeated events don't count), with
the model from the last `turn_context`. Files are parsed incrementally
(they only grow: parse from the last full line) and cached by path. A
record belongs to the run of its session whose window holds its time
(start to end; an `exec` run without an end: until the next run of the
session, at most 24 h) and to the account active then. An `exec` run ends
at its last record.

**Freshness.** A thread calls `launch::refresh_stale` for every account
every `usage.poll_idle_secs` (the per-profile lock and backoff make it
cooperate with running sessions; `AGP_NO_POLL=1` turns it off). The config
is reloaded when its mtime changes. The page polls `/api/state` every 5 s
and the other endpoints every 30 s and on a filter change.

**Page.** Colors are roles: a validated 8-slot categorical palette (light
and dark steps), status colors only for state (meters: < 60 good, < 85
warning, else critical, as in the dock), a one-hue ramp for the heatmap.
An account keeps its color slot everywhere: its config `color` when that
slot is free (`cyan` → aqua, `blue`, `yellow`, `magenta`, `green`, `red`),
else the first free slot in config order; past 8 accounts, gray. Every
chart has a tooltip on hover and keyboard focus; the history and timeline
charts have a table view; the legend hides an account (⌥-click: only
that one).

## 12. Codex (ChatGPT plans)

`agp codex [agent-prism flags] [codex args...]` runs the Codex CLI the same way
`agp` runs Claude: pick, dock, menu, switch on a limit. Code: `Tool`
(`prism_core::model`), `prism_core::codex` (login), the `CODEX_*` lists in
`share.rs`, `fetch_codex_usage` (`prism-usage`), `crates/agent-prism/src/codex.rs`
(args, thread lookup) and `SpecBuilder::build_codex`.

**Profiles.** A Codex profile is a `CODEX_HOME` dir: `~/.codex-<id>`, with
`tool = "codex"` in the config. The common home is `codex.common_home`
(`~/.codex`); for it, agent-prism removes `CODEX_HOME` (default install).
Discovery scans `~/.codex` and `~/.codex-*`; ids are `codex` and
`codex-<suffix>`. An API-key login is a proxy (usage-based billing) and is
never offered.

**Login and identity.** `<home>/auth.json` holds the ChatGPT login. agent-prism
reads the claims of `id_token` (`email`, `name`,
`https://api.openai.com/auth.chatgpt_plan_type`, `chatgpt_account_id`,
`organizations`) and the `exp` of `access_token`. It never writes
`auth.json`, never copies it and never refreshes the token: Codex rotates
the refresh token, so a copy can log the other copy out. An expired token is
`Expired`; the next `codex` run on that account refreshes it. If
`cli_auth_credentials_store` is `keyring`, there is no `auth.json` and agent-prism
cannot read the account. `agp add --codex` runs `codex login` with
`CODEX_HOME` in the embedded terminal.

**Usage.** `GET https://chatgpt.com/backend-api/wham/usage` with
`Authorization: Bearer <access_token>` and `ChatGPT-Account-Id`. The reply
has `rate_limit.{primary_window,secondary_window}` with `used_percent`,
`limit_window_seconds` and `reset_at`, plus `limit_reached`. A window of
24 h or less is the 5h window, a longer one the weekly window (Pro has only
a weekly window now). `limit_reached = true` makes the account limited
whatever the percentages say. Capacity for the balancer: pro 2.0, plus 0.4,
free and go 0.1, others 1.0. Codex accounts are balanced only against
Codex accounts.

**Share map.** Shared: `*.md` (AGENTS.md), `config.toml`, `hooks.json`,
`prompts`, `skills`, `rules`, `plugins`, `vendor_imports`, `pets`,
`memories`, `memories_v2`, `sessions`, `archived_sessions`,
`session_index.jsonl`, `history.jsonl`, `thread-writer-locks`,
`automations`, `scheduled-tasks`, `attachments`, `generated_images`,
`.personality_migration`, `.sandbox_migration`. Local: `auth.json`,
`.credentials.json`, `models_cache.json`, `version.json`, `installation_id`,
`cache`, `.tmp`, `tmp`, `log`, `app-server-daemon`, `app-server-control`,
`ipc`, `mcp-oauth-locks`, `*.sqlite*`, `.sqlite-maintenance.lock`,
`.codex-global-state.json*`. Unmanaged: `*.bak*`, `*.backup*`, `*.premerge`,
`packages`. Adopt merges `history.jsonl` and `session_index.jsonl` as a
union of lines (sorted by `ts`); any other file that differs is a conflict,
never an overlay. `sessions` must be shared for a switch to resume.

**Launch.** Args: `codex.args` + profile args + `--no-daemon` (unless the
user passes `--remote`) + `-c sqlite_home="<common>"`, put after a leading
`resume`/`fork`, else first. Env: `CODEX_HOME` and `CODEX_SQLITE_HOME`
(profile env, else the process env, else the common home). Why: the TUI
otherwise attaches to a shared app-server daemon that keeps the env of the
account that started it, and the thread list lives in SQLite
(`state_<n>.sqlite`, table `threads`), which must be the same for all
accounts so that `codex resume <id>` finds the thread. Non-interactive lines
(`exec`, `login`, `mcp`, `--help`, …: any subcommand except `resume` and
`fork`) run without the dock, like `claude -p`. `-p` is Codex's
`--profile`, not print mode.

**Session id.** Codex has no `--session-id`, and its hooks need a trust
prompt, so agp adds no hooks. Every 3 s the host runs
`/usr/bin/sqlite3 -readonly` on `state_<n>.sqlite`: the newest thread with
`thread_source = 'user'` in the work dir (`-C`, else cwd) created after the
child started, by the active `creator_account_id`. For `resume` without a
UUID (picker, `--last`, a name) it takes the thread updated most recently
after the start. A new thread (`/new`) replaces the id. Without hooks,
`busy` is always false: a limit is seen by polling only, and there is no
automatic resume prompt.

**Switch.** `codex resume <base args> <user flags> <thread id> [prompt]`,
with the subcommand, positional args and `--last` removed from the user
args. The 5 s safety net of §9 applies: if the new child exits non-zero
within 5 s, agent-prism goes back to the previous account. Status (2026-10-18):
the dock, usage, pick and launch work on a real ChatGPT Pro account; a
resume on a second ChatGPT account is not verified yet.

## 13. Later (not in v0.1)

- `token_refresh = "direct"` after verification.
- herdr: it finds agents by the foreground process name (`claude`), so a
  agent-prism pane does not show as Claude. Fix in the user's herdr fork
  (`src/detect/mod.rs` `lookup_agent`: add `"agent-prism"`), with the user's OK.
- Sync of `mcpServers` from `~/.claude.json` into each profile's
  `.claude.json`.
