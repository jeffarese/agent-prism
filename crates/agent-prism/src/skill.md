---
name: agp
description: Use agp (agent-prism) instead of plain `claude -p` or `codex exec` whenever you run Claude Code or Codex from a script, a background job or another agent, and to check the usage and limits of the user's Claude and ChatGPT accounts. agp picks the account with the most quota, can wait for a limit to reset or move to another account mid-run, and resumes the same conversation. Trigger on running `claude -p`, `codex exec`, `codex review`, spawning a headless agent, batch or parallel agent runs, "which account", "usage", "quota", "rate limit", "limit reset".
---

# agp (agent-prism)

The user runs Claude Code and Codex through `agp`, which spreads the work
over several Claude (or ChatGPT) accounts. Use it for every headless run.

## Run a headless agent

Put `agp` in front of a `claude` line, or replace `codex` with `agp codex`.
Args, stdin, stdout, stderr and the exit code stay the same; agp writes
only to stderr.

```sh
agp -p "summarize the diff"                    # instead of claude -p
git diff | agp -p "review" --output-format json
agp codex exec "fix the failing test"         # instead of codex exec
agp codex exec --json - < prompt.md > out.jsonl
agp codex review --uncommitted
```

## Per-run options

They go before the claude/codex args (after `agp` or `agp codex`) and
change this run only:

| Option | Effect |
| --- | --- |
| `--on-limit wait` | Keep the account; when it hits a limit, wait for the reset, then resume the same conversation there. |
| `--on-limit switch` | On a limit, move the conversation to the best other account and resume it. |
| `--on-limit fail` | Exit 75 at once if every usable account is limited, or when the account hits a limit. |
| `--on-limit stay` | Don't recover; the tool's own exit code. |
| `--max-wait 2h` | Wait at most this long in total (`90m`, `2h`, `1d`); longer: exit 75. |
| `--accounts a,b` / `--exclude c` | Use only / never these account ids. |
| `--max-usage 80` | Treat accounts at or over 80% (5h or weekly) as limited. |
| `--as <id>` | Use exactly this account. |
| `--policy <p>` | `smart`, `priority`, `round-robin` or `headroom` for this pick. |

Without `--on-limit`, `agp -p` replaces itself with the tool (no recovery).
With it, agp stays the parent: it drops the stdout of an attempt that a
limit stopped (except `--output-format stream-json` and `codex exec --json`,
which stream), and the resumed attempt starts with a short note plus
`continue`. Exit code 75 (`EX_TEMPFAIL`) means "gave up on a limit; try
later".
After a resume the final output is Claude's reply to that note in the same
conversation; JSON `result`, cost and usage cover the last attempt only.

Examples:

```sh
# Long unattended job: never hop accounts, just wait (at most 6h).
agp --on-limit wait --max-wait 6h -p "migrate the tests" --output-format json
# CI: fail fast instead of waiting; 75 = every usable account is limited.
agp --on-limit fail -p "review this PR" > review.md
[ $? -eq 75 ] && echo "accounts limited; retry later"
# Keep a batch off nearly full accounts and off the personal one.
agp --max-usage 70 --exclude personal --on-limit switch -p "..."
agp codex --on-limit wait exec "fix the build"
```

## Check usage before a big batch

```sh
agp status          # table: 5h and weekly usage, resets, next pick
agp status --json   # the same as JSON
agp pick --json     # the account a new Claude session would use (--codex)
agp dashboard       # usage, history and runs in the browser
```

## Inside an agp session

`AGP=1`, `AGP_PROFILE` (account id) and `AGP_LABEL` are set. A nested
`agp -p` works as usual. An interactive `agp` inside a session starts the
plain tool. Don't call `agp settings`, `agp setup`, `agp add` or
`agp adopt --apply` yourself: they change the user's accounts or are
interactive.
