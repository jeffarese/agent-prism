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

![agp starts Claude on the best account, hits the 5-hour limit, and resumes the conversation on the next account](docs/demo.gif)

The demo and the images below use fictional accounts and a stand-in for
`claude` ([demo/](demo/); `demo/record.sh` records them again).

## Install

Needs Rust 1.88 or later, and `claude` (or `codex`) on your PATH.

```sh
scripts/install.sh        # builds and installs ~/.local/bin/agp
```

Set `AGP_INSTALL_DIR` to install to a different directory.

## First run

The first `agp` starts a short setup. Nothing changes on disk until the last
step, except things that you confirm on the way.

<table>
  <tr>
    <td width="50%"><img src="docs/images/onboarding-welcome.png" alt="Welcome screen"><br>
      <b>1. Welcome.</b> What agent-prism does.</td>
    <td width="50%"><img src="docs/images/onboarding-accounts.png" alt="Accounts step"><br>
      <b>2. Accounts.</b> agent-prism finds the Claude and Codex logins on
      this Mac. Choose the ones to use and give them short names.</td>
  </tr>
  <tr>
    <td><img src="docs/images/onboarding-sharing.png" alt="Sharing step"><br>
      <b>3. Sharing.</b> Accounts share <code>~/.claude</code> (settings,
      skills, memory, sessions), so a conversation can move between them.
      Logins stay separate.</td>
    <td><img src="docs/images/onboarding-balancing.png" alt="Balancing step"><br>
      <b>4. Balancing.</b> How to pick the account for a new session, and
      what to do when an account hits its limit.</td>
  </tr>
  <tr>
    <td><img src="docs/images/onboarding-dock.png" alt="Dock step"><br>
      <b>5. Dock.</b> How the usage dock under Claude looks, with a
      preview.</td>
    <td><img src="docs/images/onboarding-done.png" alt="Done step"><br>
      <b>6. Done.</b> A summary. Enter saves the config and starts
      Claude.</td>
  </tr>
</table>

## Use

```sh
agp                     # first run: onboarding; then Claude on the best account
agp --as personal       # pin an account for this session
agp -- --help           # claude flags pass through; agent-prism flags go first
agp -p "question"       # print mode: drop-in for claude -p on the best account
agp codex               # Codex on the best ChatGPT account
agp codex --as chatgpt  # Codex on one account; codex args follow as usual
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

<table>
  <tr>
    <td width="50%"><img src="docs/images/switch.png" alt="Switch account menu"><br>
      <b>Switch account now.</b> Move the conversation to another account
      at any time.</td>
    <td width="50%"><img src="docs/images/status.png" alt="agp status output"><br>
      <b><code>agp status</code>.</b> The usage of all accounts, and the
      account that a new session would use.</td>
  </tr>
</table>

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
