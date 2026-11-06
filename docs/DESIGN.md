# agent-prism — design

`agp` starts a normal interactive `claude` session on the Claude account
with the most usable quota. It wraps the session in a PTY and keeps a small
**dock** at the bottom of the terminal that shows the usage of all accounts.
When a limit stops the active account, agent-prism moves the same conversation to
the next account (`--resume`). A hotkey opens a menu and the settings panel.

Platforms: macOS, Linux and WSL 2 (§1.1). Language: Rust 2024.

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


### 1.1 Platforms

agent-prism runs on macOS and Linux; WSL 2 is Linux with a Windows browser.
What differs lives in `prism_core::platform` or behind a `cfg`:

| | macOS | Linux, WSL |
| --- | --- | --- |
| Claude login | Keychain item (§6.1), read with `/usr/bin/security`; `<dir>/.credentials.json` as a fallback | `<dir>/.credentials.json` only (what Claude Code writes there, mode 0600); no Keychain is read |
| Running claude/codex (§4.5) | `ps` | `/proc/<pid>/{cmdline,exe,environ}` |
| Process start time (pid reuse) | `proc_pidinfo` | `/proc/<pid>/stat` + `btime` |
| Backup tar (§4.4) | bsdtar `--no-mac-metadata` | GNU tar, no flag |
| `sqlite3` (§12) | `/usr/bin/sqlite3` | the first on `PATH`; without it a Codex thread id is not found |
| Opening the dashboard | `open` | `xdg-open` (or `gio open`) with a desktop; on WSL `wslview`, else `explorer.exe`; no desktop: print the URL |

The nudge (§6.3) works over SSH on Linux, since the login is a file.
`agp doctor` prints the platform (`macOS`, `Linux`, `WSL`), says where each
login lives, and warns when `sqlite3` is missing or, on WSL, when the work dir
is on the Windows drive (`/mnt/…`). WSL is detected from the kernel release
(`microsoft`) or `WSL_DISTRO_NAME`.

## 2. Files and paths

| What | Path | Override |
| --- | --- | --- |
| Config | `~/.config/agent-prism/config.toml` | `AGP_CONFIG_DIR` (dir) |
| State root | `~/.local/state/agent-prism/` | `AGP_STATE_DIR` |
| Usage cache | `<state>/usage/<profile>.json` (+ `.lock`, `.samples.jsonl`) | |
| Run registry (claims) | `<state>/sessions/<pid>.json`, pick lock `<state>/pick.lock` | |
| Token nudges | `<state>/nudge/<profile>.json` (+ `.lock`, `empty/`) | |
| Per-run files | `<state>/run/<pid>/` (hook settings, socket `ctl.sock`) | |
| Log | `<state>/agent-prism.log` (rotated at 2 MB) | `AGP_LOG=debug` |
| Adopt journals | `<state>/adopt/<ts>-<profile>.json` | |
| Run journal | `<state>/runs.jsonl` (cut to its newest half at 8 MB) | |
| Background sessions | `<state>/bg/<session id>.json` (+ `.lock`, `.act.lock`, `.rec.lock`) | |
| Their `--settings` files | `<state>/bg/settings/<launch key>-<profile id>.json` | |
| Learning (§11f) | `<state>/learn/sessions.jsonl`, `empty.json`, `samples/<profile>.jsonl`, `accounts.json`, `state.json`, `harvest.lock` | `AGP_NO_LEARN=1` (no background harvest) |

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
run_load = 25            # % of a 5h window one working run uses per hour at capacity 1.0 (§6.6); 0 = off

[usage]
poll_active_secs = 60
poll_idle_secs = 180
token_refresh = "lazy"   # lazy | nudge | direct  (see §6.3; "direct" is not built)

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
sync_settings = true     # fold an account's own settings.json back into the common one (§4.2)
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

**Keeping `settings.json` shared.** Some of Claude's settings writes
(Claude Code 2.1.x stages them in `<profile>/.cc-writes`) rename a new
file over the link, so the change stays in that account and the accounts
drift apart (a different effort after each switch). Before Claude starts (interactive, print, background
launch and move), `settings_sync::sync` folds each such file back into
`<common>/settings.json` and links it again, under
`<state>/settings-sync.lock` (1 s, else skipped). The merge is three-way
against the account's base: `<common>/settings.json` as it was at the
last sync that found the account linked (`<state>/settings-base.json`). A
value only the account changed takes the account's, a value only common
changed keeps common's, a value both changed takes the newer file's;
objects merge key by key. The files it replaces are kept in
`<state>/settings-sync/`: `<time>-<id>.json` (the account's) and
`<time>-common.json` (common before a merge). A file with no base (it drifted
before agent-prism kept one) is left alone: there is no telling which side
changed; `doctor` lists the keys that differ and the steps to link it
again. A file equal to common is linked again at once. The common file is
written through a link (a dotfiles repo) with its mode kept. `adopt` is not
the fix for such a file: it keeps the account's values as overrides in the
overlay. `share.sync_settings = false` turns it off.

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

`tar -czf` of the profile dir (on macOS `--no-mac-metadata` to skip `._` files) plus
`<common>/history.jsonl`. Target `ssh://host/path` streams over
`ssh -o BatchMode=yes host 'mkdir -p path && cat > path/<name>.tar.gz'`, then
checks remote size > 0 with `ssh host stat`. A local target writes the file.
Name: `<YYYY-MM-DD>-<profile>-<HHMMSS>.tar.gz`.

### 4.5 Running-Claude detection

macOS: `ps -Ao pid=,comm=` for `claude` processes, then `ps -Eww -o command= -p <pid>`
(own processes only) and look for `CLAUDE_CONFIG_DIR=<dir>`. Linux: the same
from `/proc` (`argv[0]` from `cmdline`, the executable from `exe`, the
start-up environment from `environ`, own uid only). A claude with no
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
(often stale on macOS). On Linux and WSL there is no Keychain: Claude Code
keeps the same JSON in `<dir>/.credentials.json`, and that file is the
login (§1.1).

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

An expired token on an idle account is not a logout: the account stays
usable, picks treat it like any other (a reset in the past means 0%), and
the next run on it refreshes the token. `agp status`, the dashboard and the
skill call it **idle**. A login the server refused (Claude then writes the
item with no access and no refresh token) reads as no login: such an account
is never picked and shows `no login`.

`token_refresh = "nudge"` (opt-in, Claude only) keeps idle logins fresh so
that their usage can be polled. Verified 2026-11-01 with Claude 2.1.316:
`claude mcp get <unknown name>` fetches the claude.ai connector list before
it looks the name up; that fetch refreshes an expired token through Claude's
locked refresh path, then the unknown name ends the command (exit 1). It
starts no MCP server and makes no model call; tested live on one account
(token 6 h expired → valid for 8 h, usage fresh, no 5h window opened).
`claude mcp list` is not used: it health-checks every server.

- `refresh_stale` (launch picks, `agp status`, the dashboard's refresh loop,
  limit checks) starts a nudge for a pool Claude account whose token expired,
  when the last nudge of that account was 30 min ago or more
  (`<state>/nudge/<id>.json`, decided under `<id>.lock`).
- The nudge is a detached `agp _nudge <id>`: under the lock, if the token
  is still expired, it runs `claude mcp get agp-refresh-probe` with the
  account's `CLAUDE_CONFIG_DIR`, in the empty folder `<state>/nudge/empty`,
  all output dropped, killed after 60 s. Then it reads the expiry again
  (only `expiresAt`, never the secret), records before/after, and on success
  polls the account's usage at once. agent-prism still never writes the
  Keychain.
- On macOS it needs the GUI login session: over SSH the Keychain can't be
  read, the token shows as unreadable and no nudge starts. On Linux the
  login is a file, so it works over SSH too. `AGP_NO_POLL` turns it off.
- If a later Claude stops refreshing there, nothing breaks: the token stays
  expired, the log says so, and the next run refreshes it. A refused refresh
  leaves the item empty, which reads as no login (above).

### 6.4 Cache and polling

Each agent-prism process polls in a background thread. Before a fetch it reads
`<state>/usage/<id>.json`; if younger than the poll interval it uses it.
Else it takes `fs4` try-lock on `<id>.lock` (skip if another agent-prism holds it),
fetches, writes atomically, and appends `{ts,u5h,u7d}` to `.samples.jsonl`
(keep the last 7 days). Active profile: `poll_active_secs`; others:
`poll_idle_secs`. A "refresh now" request ignores the age.

A launch's refresh (`refresh_stale`, 2.5 s budget) does not skip a locked
account: it waits (up to 5 s) for the other process's fetch and then reads
its result, and it does not fetch again what another process fetched since
it looked. So lanes that start together pick on the same fresh numbers.

### 6.5 Inference and projection

- If a window's `resets_at` is in the past, its utilization is 0 and its
  reset is unknown (`inferred = true`).
- Projection: from samples in the last 60 min (5h window) or 6 h (7d window),
  weighted toward recent samples, project the utilization at reset. Needs
  ≥ 10 min of span, else none.
- Burn rate (`infer::rate_5h`): the same weighted slope of the last hour's
  5h samples after the last reset, in %/h, never negative; none with fewer
  than 3 samples, under 10 min of span, or a newest sample older than 15
  min. It counts all use of the account, also from outside agent-prism
  (claude.ai, another machine).

### 6.6 Balancer

Input: `AccountView`s for pool profiles, the model in use (for scoped
windows), the live runs per profile from the run registry (claims, below),
and each account's 5h burn rate (§6.5).

**Claims.** Every run agent-prism starts holds a record in the run registry
`<state>/sessions/<pid>.json` (`SessionRecord`: profile, tool, mode, busy,
session id, cwd, transcript, the pid's start time) while it runs, in every
mode:

| Run | Claim | Weight |
| --- | --- | --- |
| interactive `agp` (Claude) | written at the pick, kept by the host, removed at exit | 1 while it works (below), else 0.25 |
| interactive `agp codex` | the same | 0.5 (no hooks tell whether it works) |
| `agp -p`, `agp codex exec` (exec'd) | written before `exec`; the tool keeps the pid | 1 |
| supervised print (`--on-limit`) | written at the pick, moved with the run, removed at the end | 1 |
| `agp --bg` launch | from the pick until the session's record exists | 1 |
| watched background session | none: derived from `<state>/bg` (state `watching`, and job `working`, launched < 2 min ago, or background work as below) | 1 |
| background move | the watcher (or `agp bg move`) claims each target it tries, until the record says where the session runs | 1 |
| limit switch countdown/question (interactive) | the host's claim names the target until the switch or a cancel | as above |

An interactive Claude session works while a turn runs (`UserPromptSubmit`
→ `Stop`, or started or restarted with a prompt) and while its background
work runs: a turn can start subagents or a workflow and end while they go
on, often burning more than the turn did. The session's subagent and
workflow transcripts (`<transcript without .jsonl>/subagents/**`) grow as
they work, so a write there since the process started or last moved, and
within `BACKGROUND_SECS` (10 minutes, as a long tool call can pause the
writes), counts the session as working. Readers derive this from the
claim's `transcript` (from `SessionStart`), so it needs no hook. Writes from
before a move don't count: that work died with the old process.

Runs pinned with `--as` claim too. A record whose pid is gone, or whose pid
now belongs to a process that started at another time (a reused pid), is
deleted when the registry is read. Records of older versions read as busy
interactive Claude sessions.

**Learned weight.** A Claude launch first estimates how many typical runs
it counts as, `w` (§11f: similar past runs in the same repository, else
the repository's recent runs; from 1 to 4; 1 when the store knows too
little). The claim stores it as `load` (only above 1) and a working run's
weight above is `load` instead of 1 (idle stays 0.25; background work is
working). A `--bg` launch
writes it to the background record (`load`, §11d), which `running` counts
the same way. The value holds for the process's whole run: its limit
switches and supervised moves, and the watcher's and `agp bg move`'s moves
(from the record), place it with the same `w`. A pick for a heavy run says
why: "most 5h room (40% left with this run) · counts as 1.9 runs: 20
similar runs in fathom". `agp pick --n` and `capacity` plan runs of weight
1 (they know no prompt).

**Pick lock.** A pick that writes a claim holds the exclusive `flock` on
`<state>/pick.lock` from reading the claims to writing its own (5 s wait;
on timeout it picks without the lock, a launch never fails for it). The
usage refresh runs before the lock. Limit switches, supervised moves and
background moves pick under it too, so runs that hit a limit together move
to different accounts. `agp status` and `agp pick` don't take it.

**Expected load.** Per account, with `h = min(1, hours to the 5h reset)`
(1 when unknown) and `r = run_load / cap` (the rate of one working run on
this plan, `%/h`):

```
load_5h  = u5h + max(burn_rate, Σ weights of the other runs × r) × h
with_run = load_5h + w × r × h    (this run added, w its learned weight)
```

`balance.run_load` (default 25, 0 turns all of this off) is 25% of a 5h
window per working-run hour on a capacity-1.0 plan. One user's history
(2026-11-01, ~420 run-hours over 5 accounts) is consistent with it: open
sessions average ~5%/h (mostly idle: 0.25 × 25 ≈ 6), a session's first hour
~14%/h (about 40% of it working). A per-account rate learned from the
journal was tried and left out: the journal has no busy time, so it measures
open sessions, not working runs, and would undercount lanes; the burn-rate
term already adapts to what an account really uses. Per-session burn read
from the transcripts (§11f) gives each new run its weight `w` instead.

- **Limited**: `u5h >= hard`, or `u7d >= 100 - reserve_7d`, or a scoped
  window for the session's model `>= hard`. Limited accounts are never
  picked. Measured use only (not `load_5h`).
- **smart** score, highest wins:
  ```
  slack7 = max(0, 100 - reserve_7d - u7d) / 100
  days   = max(0.25, hours_to_7d_reset / 24)       (unknown reset → 7)
  score  = cap * weight * ( 0.45 * (1 - with_run/100)
                          + 0.35 * min(1, slack7 / days)
                          + 0.20 * (1 - u7d/100) )
         - (with_run >= soft_5h ? 0.50 : 0)
         - (usage unknown ? 0.15 : 0)
  cap = 2.0 for "20x" tiers, 1.0 for "5x" and team, 0.4 for pro, else 1.0
  ```
  Tie → least recently used. With `run_load = 0`, `with_run = u5h`.
- **priority**: first eligible in config order whose `with_run` stays
  under `soft_5h` (fill one, then spill); none → the first.
- **round-robin**: fewest runs (summed weights), then least recently used.
- **headroom**: max of `min(100 - with_run, 100 - reserve - u7d)`.
- **Resume affinity**: a line that resumes a conversation starts on the
  account that ran it last when that account is not limited and its
  `with_run` stays under `soft_5h` (its prompt cache is there): Claude
  `--resume <uuid>` / `-r <uuid>` / `-c` (the last session agp ran in this
  folder), Codex `resume <id>` / `exec resume <id>` / `--last`. The account
  comes from the background record, else the journal (§11b). `--as` wins.
- Every pick has a one-line reason, e.g. "weekly resets in 21h with 28%
  unused", "most 5h room (40% left with this run)", "fewest runs now",
  "resumes on the account that last ran it", "the only account not limited".
- All limited → no pick; report the earliest reset (label and profile id).
- **Plan and capacity** (`agp pick --n`, `agp status`): `plan(n)` picks
  `n` times, each pick counting the ones before (nothing is claimed).
  `capacity` is how many more working runs fit before every usable
  account's `with_run` would reach `soft_5h`, each going to the account
  with the most room (at most 50; `null` when `run_load = 0`).
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
  inside an agent-prism session (nested). Without a user session flag,
  Claude print mode gets `--session-id <uuid v4>` too, which the journal's
  start event records (a resume records the session it resumes), for resume
  affinity (§6.6) and the dashboard. The run's claim is written before the
  `exec` and stays for the tool's lifetime (same pid).
- Print mode with `--on-limit` (§11a): agent-prism stays the parent
  (`supervise.rs`) instead of exec'ing the tool.
- `--bg` among the claude args (before any `--`): a Claude Code background
  session, picked and set up like print mode, plus a watcher (§11d).
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
  The status row drops its trailing parts (usage age, account, session) to
  fit, and the countdown shortens to `→ Acme Bea in 4s · ⌃\ stay`.
- Compact shows the 5h window only:
  ` ● Acme Al 5h 48% │ ○ Acme Bea 5h 1% ▸ │ Σ 5h 24% │ ⌃\ `. Narrow terminals
  drop the `5h` names, then the pool, then the labels (` ● 48% │ ○ 1% ▸ │ ⌃\ `).
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

1. Target = balancer pick excluding the current account (or the user's),
   under the pick lock with fresh claims (§6.6). The session's claim names
   the target from now until the switch, or goes back on a cancel or a
   refused switch. None → status message with the earliest reset; stop.
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
   `/goal` (the latest `goal_status` attachment is neither met nor failed),
   or background work runs (subagents or a workflow whose turn ended, §6.6:
   the restart stops them, and without a prompt nothing restarts them).
   Claude Code restores the goal on `--resume`, but its Stop hook only acts
   when a turn ends; without a prompt the resumed session waits for input.
   The prompt is a one-line note (account, limit, and that background
   tasks, subagents and workflows of the old process have stopped), a blank
   line, then `resume_prompt`. A manual switch sends the same note when
   background work runs. A restart with a prompt counts as working (§6.6)
   until its first turn ends, so sessions that move together spread. The transcript scan reads at most the last 64 MB.
   Claude starts a resumed session on the default effort, so an effort the
   session chose in Claude since agent-prism launched it (the last
   `/effort` or `/model` picker output in the transcript, newer than the
   launch) comes back as `--effort <level>` in place of the launch's
   `--effort`; `/effort auto` drops the launch's. This holds for every
   restart with `--resume`, the wait on the same account too. The user
   args are read the way Claude's parser reads them (values of
   `--effort`, list flags such as `--add-dir a b`, optional values), so a
   flag's value is never taken for the prompt.
6. If the new child exits non-zero within 5 s, show the error and offer
   "go back".
7. Requires shared `projects/` in both profiles; else the switch item is
   disabled with "run agp adopt <id>".

Run registry `<state>/sessions/<pid>.json` (`{pid, profile, session_id,
cwd, started_at, switched_at, tool, mode, busy, pid_start}`; all runs, not
only interactive ones, §6.6); records of dead or reused pids are removed.
It gives "used by another agent-prism", the balancer's runs and load, the
dashboard's live runs and `agp status`'s RUNS.

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
agp status [--json] [--watch]    RUNS per account, room left; JSON: views[].runs/load_5h/usable, capacity
agp pick [--json] [--codex] [--n <k>]   --n: where k new runs would go (nothing claimed)
agp add [--codex | --claude] [id]
agp settings | setup | doctor
agp dashboard [--port <n>] [--no-open] [--allow-host <name>]
agp skill [show]                print the agp skill (SKILL.md; §11e)
agp skill install [--agent <a>] [--dir <path>]
agp skill list | uninstall [--agent <a>] [--dir <path>]
agp adopt <id|dir> [--apply] [--no-backup] [--force]
agp exec <id> -- <cmd...>       run a command with that profile's env
agp [opts] --bg [claude args]   a Claude Code background session (§11d)
agp bg [--json]                 background sessions and their watchers
agp bg watch <session> [--as <id>] [--policy <p>] [per-run opts]
agp bg move <session> [--to <id>] [--json]
agp bg _watch <session id>      internal (the detached watcher)
agp _nudge <id>                 internal (a detached token nudge, §6.3)
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
an interactive session is an error. Print mode without `--on-limit` (and
`--bg`) applies them to its pick too.

With `--bg` (§11d) the effective `--on-limit` is `switch`, `wait` or
`stay`: `ask` → `switch` (nobody can answer), `fail` → `stay` (no process
of agent-prism to fail); no flag → the config's `on_limit` (`auto`, `ask`
→ `switch`). `--max-wait` caps a wait for a reset; past it the session
stays limited.

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
supervised print mode, `bg` = a background session: the launch writes
`start`, the watcher writes the rest under the same run id, `end` when the
job is gone), `profile`, and when they apply `from`, `limit`
(`5h | 7d | scoped`), `until`, `reason`, `session`, `cwd`, `exit`, `secs`.
A line is one `write` in append mode, so processes don't interleave. Errors
are logged and never fail a run. A run is live while its claim is in the
run registry (all modes, §6.6), so the dashboard sees running print runs.

## 11c. Dashboard (`agp dashboard`)

Code: `crates/agent-prism/src/dashboard.rs` and `dashboard/` (the page is
`index.html`, `app.css`, `app.js`, embedded with `include_str!`).

**Server.** `std::net::TcpListener` on `127.0.0.1:<port>` (0 = any free
port), a thread per connection (at most 32), one request per connection.
Only `GET`. The `Host` header must be `127.0.0.1:<port>` or
`localhost:<port>` (DNS rebinding), or a name given with `--allow-host`,
with or without a port (what a reverse proxy such as `tailscale serve`
forwards), and a request with `Sec-Fetch-Site: cross-site | same-site` is
refused. No CORS headers. The page has a strict
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

## 11d. Background sessions (`claude --bg`)

Code: `crates/agent-prism/src/bg/` (`job.rs` Claude's job files and output,
`record.rs` the record and locks, `claude.rs` running claude and the
detached spawn, `mover.rs` stop and resume, `watch.rs` the watcher,
`mod.rs` the commands).

`claude --bg …` hands the session to Claude Code's per-account supervisor
and exits within a second, so agent-prism is not the parent of the session
and the dock (§9) and supervised print mode (§11a) can't see its limits. A
detached watcher process does. Facts (Claude Code 2.1.316):

- Each profile (config dir) has its own supervisor and
  `<config dir>/jobs/<short>/state.json` per session (`short` = first 8
  hex of the session id): `state` (`working | blocked | done | …`),
  `detail`, `needs`, `sessionId`, `cwd` (where it was launched),
  `worktreePath`, `respawnFlags` (the launch flags), `updatedAt`,
  `createdAt`. `claude agents --json --all` in the profile's env lists the
  sessions with `id`, `sessionId`, `pid` (absent when the process is gone).
- A rate limit sets `state: blocked`, `needs: "rate limited — wait and
  retry · <detail>"`, `detail: "You've hit your 5-hour limit · resets
  6:10am (Europe/Madrid)"`. The process stays alive, idle; it does not
  retry.
- The launch prints `backgrounded · <short> · <name>` (with escape
  sequences); a wake prints `woke session <short> with its saved options`.
- `claude --bg --resume <id> <flags…> "<prompt>"` in a profile with no job
  for the session resumes the same session (same id) with its whole
  conversation, back in its worktree. It needs the transcript visible
  (shared `projects/`) and the folder trusted. In a profile that already
  has the job, any flag (and a resume while the old process runs) starts a
  copy under a new id that shares the worktree; there the no-flag form
  `claude --bg --resume <id> "<prompt>"` wakes the stopped job under the
  same id. `claude rm` deletes the job and its worktree: agent-prism never
  runs it (the moved session still uses the worktree).

**Launch** (`agp [opts] [--] --bg <claude args…>`, Claude only). The pick,
env, args and `--settings` (overlay, memory dir) are print mode's (§7.4),
with the per-run options, except where the settings go (below); trust is
synced (§4.7). Never the wrapper, and the
nested-session check does not apply (`AGP=1` changes nothing). claude runs
with stdin and stderr inherited and stdout copied through as it comes (to
read the short id); agent-prism waits for claude to exit, reads the pipe
for at most 1 s more (a supervisor claude started may hold it) and exits
with claude's code. A non-zero exit: nothing else. Else: the short id from
the output (else the newest job of the profile created for this cwd since
the launch), the session id from its `state.json` (waiting up to 5 s, else
`claude agents`), a journal `start`, the record, and, unless the
effective on-limit is `stay`, the watcher. A problem there is one stderr
note; the exit code stays claude's.

**Settings files.** Claude Code saves a job's flags (`respawnFlags`) and
reuses them on `claude respawn` and on a wake, so inline JSON in
`--settings` would freeze the settings of the launch: an orchestrator that
rewrites its `--settings` file before a respawn would never reach the
session again. So when agent-prism merges settings for a background
session (the overlay and/or the memory dir, with or without a user
`--settings` file), it writes the merge to
`<state>/bg/settings/<key>-<profile id>.json` (atomically) and passes that
path. `key` is the launch's (made before claude prints the session id,
kept in the record's `launch.key`); the path stays the same for a session
and profile, since that profile's job keeps it. A user value that is
inline JSON, or nothing to merge (the user's file or the overlay alone),
goes as print mode passes it. The file is written again from the user's
value (a relative path made absolute against the launch folder, read
fresh), the profile's overlay and memory dir: before every resume or wake
in a profile (the flags form writes the target's file; the wake form
rewrites the file its saved flags name), and in `agp bg watch` for the
session's current profile before it returns. A user file that can't be
read or merged leaves the old file as it is. The files go with the record.

**The watcher** (`agp bg _watch <session id>`). Spawned with `setsid` (a
new session and process group, so a timeout that kills the launch's group
leaves it alone), stdin/stdout/stderr on `/dev/null` and every other
inherited descriptor closed at exec; nobody waits for it. It logs to the
agent-prism log. One per session: it holds `<id>.lock` (`flock`) for its
life; a second one gives up after 2 s. Every round (10 s; 60 s after 10
minutes without a change; `AGP_BG_POLL_MS` for tests) it reads the record
and the config (with the watch's options), stats the job's `state.json`
in the record's profile and parses it only when its mtime changed. It
does not run `claude agents` in the loop (only to act, and once per
change of an old account's job, below).

- The job folder gone in the current profile: state `ended`, journal
  `end`, exit. No change for 7 days (counted from the watch's start at the
  earliest): `dormant`, exit (`agp bg watch` re-arms it).
- A job of the session in a previous profile changed more than 2 s after
  the current one's and `claude agents` there shows it with a live pid
  (someone ran `claude respawn` in the old account; checked once per such
  change): the record follows it there. A move writes the old job (the
  stop) before the new one, so a move never looks like this.
- A new limit (rate limited, `updatedAt` not handled yet; each is handled
  once): refresh that profile's usage (if older than 30 s); the window
  and reset come from the usage when it shows the account limited, else
  from the text (`resets 6:10am (Europe/Madrid)`, `resets Nov 3, 6pm
  (UTC)`); record `limit`, journal `limit`. Then per on-limit: `stay`
  → state `limited`; `switch` → move now (below) to the best target; no
  target or every attempt failed → wait as `wait` does, and every 5 min
  look again for a free account; `wait` → state `waiting` until the reset
  + 30 s (unknown: check again in 5 min), journal `wait`; past
  `--max-wait` (from the limit's time) → `limited`.
- The wait is over: if the refreshed usage shows a later reset, wait
  again; else wake the session on the same account (below, no targets).
  A failed wake retries in 5 min.
- The job no longer rate limited while `limited` or `waiting` (it went on
  by itself, e.g. someone woke it): back to `watching`; a newer
  `updatedAt` that is still a limit is a new limit.
- A record left `moving` with the action lock free (a move died): back to
  `watching` and the limit is looked at again.
- `agp bg watch` with another on-limit than recorded: the current limit is
  planned again.

**Targets.** The balancer's picks (§6.6) over the pool with the watch's
`--accounts`/`--exclude`/`--max-usage`/`--policy`, without the current
profile, repeated to a list best first; only profiles whose `projects`
resolves to the same dir as the current one's and that trust the job's
`cwd` (after a trust sync, §4.7; a trusted parent counts up to the git
toplevel). `agp bg move --to` takes that one account (limited or out of
the pool too) after the same two checks.

**Moving a session** (`mover::relocate`; the watcher and `agp bg move`):

1. Record state `moving` (atomic write) before anything stops.
2. `claude stop <short>` in the current profile; wait (≤ 20 s) until
   `claude agents` there shows no live pid (between the checks agent-prism
   watches the known pid itself). Still running: give up (a resume would
   start a copy).
3. For each target in order (a wake: the current profile): check once
   more that `claude agents` shows no live pid in the current profile, in
   every profile tried so far and in the target. One has: abort, the
   record goes back to `watching` on the profile where it runs (a move if
   that is another profile), logged; the limit is not handled again.
4. Resume in the target's env with cwd = the job's `cwd`: the flags form
   with the old job's `respawnFlags` when the target has no
   `jobs/<short>`, else the no-flag form. For a session agent-prism
   launched, the flags form takes out the args agent-prism added for the
   old account (`[claude] args` and the profile's `args`, as recorded in
   `launch.added` for that profile when its job was started), puts the
   target's in front (recorded for the target) and passes the target's
   settings file (above), and an effort chosen in the session since its
   record was made comes back as `--effort` (§9 step 5). The wake form takes
   no flags, so the woken job keeps the effort of its own launch. The prompt: a one-line note ("This session moved from
   account A to B after the 5-hour limit" / "on request" for
   `agp bg move` / "agent-prism waited for the reset on the same account
   (A) after the 5-hour limit"; "the old process's background tasks and
   subagents stopped, so start again the ones you still need"), a blank
   line, then `balance.resume_prompt` (`continue` when empty).
5. The output must name the same short id; another id is a copy: stop it,
   and the attempt failed. Then `claude agents` in the target must show
   the session with a live pid within 15 s. Only then the record leaves
   `moving`: `watching` on the target with the move in `moves`, journal
   `switch` (a wake: journal `resume`). A failed attempt tries the next
   target.
6. Every attempt failed: the watcher waits (state `waiting`); `agp bg
   move` wakes the session where it was (same steps, no targets) and exits
   non-zero.

The whole stop-and-resume runs under `<id>.act.lock`, which the watcher
takes when it acts (and skips a round while someone else holds it) and
`agp bg move` holds for its whole run, so they never race.

**Record** `<state>/bg/<session id>.json`, written atomically under
`<id>.rec.lock` (read-modify-write). `agp bg --json` prints
`{"sessions": [<record>…]}`, newest change first; another program
(chief) reads it, so these keys are a contract:

```json
{"session_id": "…", "short_id": "f1de9de1", "name": "…", "profile": "laura", "label": "PB4",
 "config_dir": "/Users/x/.claude-laura", "cwd": "/…", "on_limit": "switch",
 "state": "watching|limited|waiting|moving|ended|dormant",
 "limit": {"text": "You've hit your 5-hour limit · resets 6:10am (Europe/Madrid)",
           "since": 1793500000, "resets_at": 1793510000, "window": "5-hour limit"},
 "moves": [{"at": 1793500100, "from": "cpb", "from_label": "PB2", "from_config_dir": "/…/.claude-cpb",
            "to": "laura", "to_label": "PB4", "to_config_dir": "/…/.claude-laura", "reason": "5-hour limit"}],
 "previous": [{"profile": "cpb", "label": "PB2", "config_dir": "/…/.claude-cpb"}],
 "watcher_pid": 1234, "watching": true, "updated_at": 1793500100}
```

`config_dir` is null for the default install; `limit` is null when not
limited; `resets_at` null when unknown; times are unix seconds.
`moves[].reason`: `5-hour limit`, `weekly limit`, `model limit`, `limit`
(window unknown) or `manual move`. `previous`: every profile the session
ran in before the current one, oldest first, without repeats and without
the current profile (where the stopped old jobs are). `watching`: a
watcher holds the lock now. Extra keys: `created_at`, `wait_until`,
`note` (the last thing that happened), `options` (the watch's per-run
options), `run` (journal run id), `launch` (`user_settings`, `key`,
`added`: what agent-prism launched with), `limit_key`, `load` (the
learned weight of the launch, §6.6; absent: 1). A launch with
on-limit `stay` writes the record too (no watcher, `watching: false`). A
record nobody watches whose job folder is gone becomes `ended` when
listed; records (and settings files) of sessions that ended a week ago
are removed when listed.

**`agp bg watch <session id | short id> [--as <id>] [--policy <p>] [per-run
opts]`**: the profile is `--as`, else the one whose `jobs/` has the short
id (the newest `state.json` when several do: the old accounts keep their
stopped jobs). It writes the record with the given options (replacing the
old ones; the on-limit from the config when none is given) and starts the
detached watcher unless one runs (which reads the new options in its next
round). It writes the session's settings file for that profile again
first (above). A session agent-prism did not launch gets a journal run of
its own. Exits 0 at once; an orchestrator calls it right before (for the
settings) or after `claude respawn`.

**`agp bg move <session> [--to <id>] [--json]`**: moves now, without a
limit, in the foreground within about 60 s (the watch's options apply to
the target choice). `--json` prints `{"moved": true, "session_id",
"short_id", "from", "from_label", "to", "to_label", "to_config_dir"}`;
a failure exits 1 with the reason on stderr.

## 11e. Skills (`agp skill`)

Code: `crates/agent-prism/src/skill.rs`; the skill is
`crates/agent-prism/skills/agp/` (`SKILL.md` plus `reference/lanes.md`,
`background.md`, `status.md`), embedded with `include_str!` (`FILES`), so
an install always matches the binary. A test checks that `FILES` and the
folder hold the same files, that `SKILL.md` links every reference page and
that the frontmatter follows the Agent Skills rules (only `name` and
`description`; `name` = folder name, lowercase, ≤ 64; description ≤ 1024
characters; Codex rejects other keys).

**Targets** (`--agent` names; confirmed in each agent's docs, and for
Claude Code 2.1.316 and Codex 0.165.0 in the binaries):

| Agent | Skills folder | Auto |
| --- | --- | --- |
| `claude` | `<common_home>/skills` | common home exists |
| `codex` | `<codex.common_home>/skills` | common home exists |
| `gemini` | `~/.gemini/skills` | `~/.gemini` exists |
| `copilot` | `~/.copilot/skills` | `~/.copilot` exists |
| `droid` | `~/.factory/skills` | `~/.factory` exists |
| `agents` | `~/.agents/skills` | never (only `--agent agents`) |
| `amp`, `opencode`, `cursor`, `goose` | Claude's folder (they read `~/.claude/skills`) | no own copy |

One copy per folder an agent reads, so an agent never lists the skill
twice: Amp, OpenCode, Cursor and Goose read `~/.claude/skills`, and Codex
reads `~/.agents/skills` besides `$CODEX_HOME/skills` (which its source
marks deprecated; if Codex drops it, `codex` moves to `~/.agents/skills`).
Sources: code.claude.com/docs/en/skills, learn.chatgpt.com/docs/build-skills
(and `codex-rs/ext/skills/src/host_roots.rs`), geminicli.com/docs/cli/skills,
docs.github.com (Copilot CLI, add skills), docs.factory.com/cli/configuration/skills,
ampcode.com/docs/customize/skills, opencode.ai/docs/skills,
cursor.com/docs/context/skills, goose-docs.ai (using skills).

**Commands.** `agp skill [show]` prints `SKILL.md`. `install` without
names takes the auto targets that are detected; `--agent` (repeatable,
comma lists) takes those, creating the folder; `--dir <path>` takes
`<path>/agp` (the path must exist). `list` (`status`) prints each known
target, its folder and its state (`current`, `outdated`, `not installed`,
`not agp's`, a link, `reads the claude copy`). `uninstall` without names
takes every agent's own folder. One line per target: `installed |
updated | up to date | removed | not there | skipped | failed` and the
path. Exit 1 when a target was skipped or failed.

**Ownership.** agp writes `.agp-skill.json` (`{"skill": "agp", "by":
"agent-prism", "files": [...]}`) into `<skills dir>/agp`. A folder is
agp's when it has that manifest, holds only an agp `SKILL.md` (the
one-file layout of older agp versions), or is empty. Anything else is left
alone (skipped).

**Install rules.**

- Each file is written to a temp file in its folder and renamed, only
  when its content differs. The manifest goes last.
- Files the old manifest (or the one-file layout) lists that the skill no
  longer has are removed, then their folders left empty; only plain
  relative paths inside `<skills dir>/agp` count. Files agp did not write
  stay.
- Symlinks are never followed out of the target: a symlinked folder inside
  `agp/` fails that target; a symlinked file is replaced by the new file.
  `<skills dir>/agp` as a symlink is skipped unless it points at a folder
  agp manages; then agp writes into that folder, once per run (folders are
  compared by canonical path, so two targets that reach one folder print
  `up to date (same folder as …)`).
- Uninstall removes the files agp wrote and the manifest, then the folder
  if nothing else is in it; a symlink is left as it is.

## 11f. Learning (`agp learn`)

Code: `crates/agent-prism/src/learn/` (`scan.rs` reads transcripts,
`store.rs` the store, `estimate.rs` a new run's weight, `report.rs` the
output). The goal: know how heavy a repository's kinds of task are, from
past sessions, so that the balancer weighs a new run by its repository and
task instead of the flat `run_load` (§6.6), and keep the data to analyze
it. Nothing leaves the machine.

**A new run's weight** (`estimate.rs`, read from `sessions.jsonl` at every
Claude launch, before the pick). Typical = the median burn
over all stored sessions. The repository's runs of the last 90 days with 3+
working minutes count. The prompt (the claude args' positionals and
everything after `--`) and each past first prompt become sets of lowercase
words of 2+ letters or digits from their first 200 characters; past runs
whose sets share at least half of all their words (Jaccard ≥ 0.5) are
similar. With 3 or more similar runs (the 20 most similar, newest among
equals), the run's rate is their p75 ("20 similar runs in fathom"); else,
with 5 or more runs in the repository, the median of the newest 50 ("50
recent runs in fathom"); else no estimate. Weight = rate / typical, clamped
to 1–4 and rounded to 0.1: a run never counts as less than one (an
underestimate costs a move, which stops background work; an overestimate
only spreads runs more). Checked on the m2max store (2026-11-02): fathom
lane runners 1.9, any other fathom run 1.8, tri and garmin-watchfaces 1.

**Harvest.** Reads every Claude session under the `projects/` folders (the
common home's and each Claude profile's, once each after `canonicalize`)
whose transcripts changed since the last harvest (size and newest mtime of
the main file and all agent files), and rewrites the store. A session id
found in two folders (a renamed project folder) is read from the larger
copy. Sessions with no model response are not stored; their fingerprints
in `empty.json` keep them from being read again. Summaries stay after
Claude Code deletes the transcripts (`cleanupPeriodDays`, 30 by default),
for 400 days after the session's end. It then fills `agp` from the run
journal (§11b): the run, mode, accounts in order and limits hit, for each
session a journal line names (a later run that resumed the session adds its
accounts). It also appends the usage cache's samples (§6.4, kept 7 days)
to `samples/<profile>.jsonl`, one per 5 minutes (the last of each ended
step), kept 400 days, and writes each Claude account's plan (label,
subscription, rate-limit tier, capacity) to `accounts.json` when its
credentials can be read. A full read of ~600 sessions (5 GB) takes about
4 s; an unchanged store about 0.1 s.

**When.** `agp learn` harvests in the foreground (waiting up to 120 s for a
running harvest, else it shows what is stored). A launch pick that takes
the pick lock (§6.6) starts `agp _learn` detached when the last harvest
started 6 h ago or more and no harvest holds `harvest.lock`. Off with
`AGP_NO_LEARN` or `AGP_NO_POLL`.

**Reading a session** (`scan.rs`). The format is Claude Code's and not
documented; lines that don't parse are skipped. Files: the main
`<session>.jsonl`, plain agents `<session>/subagents/*.jsonl` and workflow
agents `<session>/subagents/workflows/<run>/*.jsonl`. Only `assistant`
lines, `ai-title` lines and `user` lines up to the first prompt are
parsed. Claude Code writes one line per content block of a response and
repeats the response's `usage` on each (checked with Claude Code 2.1.312),
so usage counts once per `message.id` (else `requestId`), the larger value
of each count. `<synthetic>` responses and empty usage don't count.

**A summary** (`sessions.jsonl`, one JSON line each, `v` = 1; an older `v`
is read again while its transcripts exist):

| Field | Meaning |
| --- | --- |
| `session`, `cwd`, `branch` | first `cwd` and `gitBranch` of the main transcript |
| `repo` | the folder holding `.git`; a worktree's `.git` file points to its main work tree; a folder that is gone: the part before `/.claude/worktrees/`, else the folder |
| `start`, `end` | first and last model response, all agents |
| `work_secs` | sum of the gaps of at most 5 min between responses, all agents |
| `entry`, `effort`, `model` | `entrypoint` (`cli`, `sdk-cli`, `claude-desktop`, ...), the main conversation's most used `effort`, its model with the most kU |
| `prompt`, `title` | the first prompt typed (whitespace collapsed, 200 characters; local command output, reminders and caveats skipped), Claude Code's `aiTitle` |
| `tokens` | per model: `input`, `cache_write`, `cache_read`, `output` |
| `ku`, `main_ku` | kU of all agents and of the main conversation |
| `agents`, `workflows`, `workflow_agents` | counts of agent transcripts and workflow runs |
| `tools` | the main conversation's tool calls by name |
| `peak30_ku`, `buckets` | most kU in 30 min; `[start, kU]` per 5 min, to set against the archived usage samples |
| `agp` | `{run, mode, accounts: [[since, profile]], limits}` from the journal |
| `src_bytes`, `src_mtime` | the transcripts' fingerprint when read |

kU (thousand input-token equivalents) = (input + 1.25 × cache write + 0.1
× cache read + 5 × output) / 1000, the API's price ratios, the same for
every model. It ranks sessions; turning it into % of an account's 5h window
needs the archived samples and the accounts' plans (a later step).

**Kinds**: `workflow` (a workflow run or a `Workflow` call), `5+ agents`,
`1-4 agents`, `solo`. Burn = kU per working hour, for sessions with 3+
working minutes.

**`agp learn [<path>] [--prompt <text>] [--all] [--json]`**: harvests, then
shows the repository of `path` (default: the current folder, through
`repo_root`): the weight a new run there gets (with `--prompt`: a run with
that prompt) and why, the sessions, their date range, the median burn over all repositories
("typical"), a table by kind and by effort (sessions, median, p75, p90 by
nearest rank, median × typical, median working time) and the latest 12
sessions (start, kind, effort, working time, burn, accounts and limits from
the journal, title or prompt). `--all`: one line per repository, most
sessions first. `--json`: the same data (`RepoReport`, with `new_run`
`{weight, why, prompt}` or null, or a list of `RepoLine`). `learn` is a reserved word like `status` (with `-p` the line
goes to claude).

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
`sqlite3 -readonly` (§1.1) on `state_<n>.sqlite`: the newest thread with
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
