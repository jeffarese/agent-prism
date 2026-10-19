# agent-prism

`agp` starts Claude Code on the Claude account with the most usable quota,
and keeps a dock with the usage of all your accounts below the normal Claude
screen. When a limit stops the active account, agent-prism moves the same
conversation to the next account (`claude --resume`).

`agp codex` does the same for the Codex CLI with ChatGPT plans (Plus, Pro,
Business): it starts `codex` on the ChatGPT account with the most quota left
and resumes the thread on another account (`codex resume`). A session uses
the accounts of one tool only.

Platform: macOS only. agent-prism reads Claude logins from the macOS
Keychain.

```
 ▐▛███▛█   Claude Code
 ...                                   (the normal Claude Code screen)
 ❯
──────────────────────────────────────────────────────────── agent-prism · smart ─
 ● Acme Al   team  5h ███████▌░░░░░░░  48% ↻2h14m  7d ████▊░░░░░░░░░░ 31% ↻3d4h
 ○ Acme Bea  team  5h ▏░░░░░░░░░░░░░░   1%         7d ██████████▉░░░░ 72% ↻21h  next ▸
 ⌃\ menu · session 3f2a91c0 · on Acme Al since 10:42 · usage 40s ago
```

## Install

Needs Rust 1.88 or later, and `claude` (or `codex`) on your PATH.

```sh
scripts/install.sh        # builds and installs ~/.local/bin/agp
```

Set `AGP_INSTALL_DIR` to install to a different directory.

## Use

```sh
agp                     # first run: onboarding; then Claude on the best account
agp --as acme-bea       # pin an account for this session
agp -- --help           # claude flags pass through; agent-prism flags go first
agp -p "question"       # print mode: drop-in for claude -p on the best account
agp codex               # Codex on the best ChatGPT account
agp codex --as work     # Codex on one account; codex args follow as usual
agp status              # usage table of all accounts (--json, --watch)
agp pick --codex        # the ChatGPT account a new session would use
agp settings            # settings, full screen
agp add [id]            # add an account; asks Claude or Codex, then logs in
agp add --codex [id]    # add a ChatGPT account (creates ~/.codex-<id>)
agp adopt <id>          # show how a profile dir would share ~/.claude (--apply)
agp doctor              # health check
```

agent-prism flags (`--as`, `--policy`, `--dock`) only count before the claude
args. Any `claude -p ...` line works as `agp -p ...`: same args, stdin,
stdout and exit code. agent-prism never prompts there; if it can't pick an
account it says so on stderr and runs claude anyway.

In a session, `⌃\` opens the menu: switch account now, settings, accounts,
refresh usage, dock mode, log. Press `⌃\` twice to send `⌃\` to Claude
(or Codex).

## How it works

- Each account is a Claude config dir (`~/.claude-<id>`). Shared parts
  (CLAUDE.md, settings, agents, skills, hooks, plugins, projects, history …)
  are symlinks into `~/.claude`; account parts (`.claude.json`, credentials,
  daemon, caches) stay in the dir. `agp adopt` sets this up with a backup
  first.
- Usage comes from the same endpoint Claude Code uses, with each account's
  token from the Keychain. agent-prism never prints or stores tokens and never
  writes to the Keychain.
- agent-prism points Claude's auto memory at the real `~/.claude/projects/...`
  path, so memory writes through the shared symlink don't prompt.
- agent-prism adds its hooks with `claude --settings`; your `settings.json` does
  not change.
- A Codex account is a `CODEX_HOME` dir (`~/.codex-<id>`). Its login
  (`auth.json`) stays in the dir; `config.toml`, `AGENTS.md`, skills,
  prompts, memories and the session files are symlinks into `~/.codex`.
  All accounts use the thread database in `~/.codex` (`sqlite_home`), and
  agent-prism starts Codex with `--no-daemon`, so each account uses its own login.
  Usage comes from the endpoint behind Codex's `/status`. agent-prism finds the
  thread id in Codex's database (read-only).

Config: `~/.config/agent-prism/config.toml`. State and logs: `~/.local/state/agent-prism/`.
Design: [docs/DESIGN.md](docs/DESIGN.md).

## Caveats

- agent-prism is not affiliated with Anthropic or OpenAI.
- The usage numbers come from endpoints that Claude Code and Codex use
  internally. They are not documented, and they can change or stop working
  at any time.
- Use only accounts that you are allowed to use, and follow the terms of
  your Claude and ChatGPT plans.

## License

MIT. See [LICENSE](LICENSE).
