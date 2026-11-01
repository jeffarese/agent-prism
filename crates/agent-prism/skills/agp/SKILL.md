---
name: agp
description: Run Claude Code and Codex through agp (agent-prism), which spreads agent runs over the user's Claude and ChatGPT accounts and handles usage limits. Use it instead of plain `claude -p`, `claude --bg`, `codex exec` or a new interactive `claude`/`codex` whenever you start an agent from a script, a background job, a terminal pane or another agent, and when you plan parallel or fan-out agent lanes. Also use it to check usage, quota, limits, resets and accounts (`agp status`), and before telling the user a login expired. Trigger on claude -p, claude --print, claude --bg, codex exec, codex review, headless or background agents, parallel lanes, fan-out, batch runs, "which account", usage, quota, rate limit, 5-hour or weekly limit, limit reset, exit code 75, expired or idle login.
---

# agp (agent-prism)

The user runs Claude Code and Codex through `agp`. agp starts each run on
the account with the most room, spreads parallel runs over the accounts,
and moves or waits when an account hits a limit. Use `agp` for every agent
you start.

## Start a run

Put `agp` in front of a `claude` line. Write `agp codex` for `codex`.
Args, stdin, stdout, stderr and the exit code stay the same. agp writes
only to stderr.

```sh
agp -p "summarize the diff"                      # instead of claude -p
git diff | agp -p "review" --output-format json
agp codex exec "fix the failing test"           # instead of codex exec
agp codex exec --json - < prompt.md > out.jsonl
agp codex review --uncommitted
agp --bg --name fix-auth "fix the auth bug"     # instead of claude --bg
```

## Continue a conversation

`agp -p` gives every Claude run a session id. With `--output-format json`
it is in `session_id`. Resume with it:

```sh
sid=$(agp -p "plan the migration" --output-format json | jq -r .session_id)
agp -p --resume "$sid" "now write the tests"
agp codex exec resume <thread id> "go on"
```

A resume starts on the account that last ran that session, when that
account is not limited and has room. `-c`/`--continue` takes the last
session agp ran in this folder. `--as` still wins.

## Run lanes in parallel

Start every lane without `--as`. agp spreads the lanes over the accounts,
even lanes started in the same second.

```sh
agp status --json | jq .capacity          # {"claude": 5, "codex": 2}
for t in api ui docs; do
  agp --on-limit switch -p "do the $t part" > "out-$t.md" &
done
wait
```

- Read `capacity` before a big fan-out. It is how many more runs the pool
  takes now before every account would reach its soft 5h mark. Need more lanes
  than that? Tell the user. Extra lanes share accounts and hit limits
  sooner. Run them in batches instead.
- `agp pick --n 6` shows where 6 new runs would go. It claims nothing.
- Use `--as <id>` only when the user names an account. Never pin lanes to
  the account that looks emptiest: agp already counts every live run.

Details, pane lanes and examples: [reference/lanes.md](reference/lanes.md).

## Long runs and limits

Per-run options go right after `agp` (or `agp codex`), before the tool's
args. They change this run only.

| Option | Effect |
| --- | --- |
| `--on-limit switch` | On a limit, move the conversation to another account and resume it. |
| `--on-limit wait` | Keep the account. Wait for the reset, then resume there. |
| `--on-limit fail` | Exit 75 when every usable account is limited, or when the account hits a limit. |
| `--on-limit stay` | Do nothing on a limit. You get the tool's exit code. |
| `--max-wait 2h` | Wait at most this long in total (`90m`, `2h`, `1d`). Longer: exit 75. |
| `--accounts a,b` / `--exclude c` | Use only / never these account ids. |
| `--max-usage 80` | Treat accounts at or over 80% (5h or weekly) as limited. |
| `--as <id>` | Use exactly this account. Only when the user names it. |
| `--policy <p>` | `smart`, `priority`, `round-robin` or `headroom` for this pick. |

- A lane that runs long: use `agp --bg ...` (a watcher moves or wakes it on
  a limit), or `agp --on-limit switch -p ...`, or
  `agp --on-limit wait --max-wait 6h -p ...`.
- An interactive lane in a terminal tab or pane (Rex, herdr, tmux):
  `agp --dock compact -- <claude args> "<prompt>"`. It moves itself to
  another account on a limit.
- Without `--on-limit`, `agp -p` turns into the tool and does nothing on a
  limit.
- Exit code 75 means agp gave up on a limit (or every usable account is
  limited, with `--on-limit fail`). The task did not fail. Retry later.
- After a resume, the output is Claude's reply to a short note and
  `continue`. JSON `result`, cost and usage cover the last attempt only.

Background sessions: [reference/background.md](reference/background.md).

## Accounts and logins

```sh
agp status          # 5h and weekly use, resets, RUNS, state, next pick
agp status --json   # the same for scripts: views[], capacity, picks
```

- `idle` (older agp: `expired`): agp's copy of the token ran out because
  nothing used the account for hours. The account is still logged in and
  usable. Claude refreshes the token on the next run there. Don't ask the
  user to log in. Don't avoid the account.
- `no login`, or a run that fails with an authentication error: only then
  the user must log in (`agp add`, or `/login` in that account). Tell them.
- `limited`: agp skips it until the reset.
- In JSON, read `usable`, not `token`.

Fields and troubleshooting: [reference/status.md](reference/status.md).

## Don't

- Don't run `agp settings`, `agp setup`, `agp add` or `agp adopt --apply`.
  They are interactive or change the user's accounts. Ask the user.
- Never `claude rm` a session agp moved. The moved session still uses its
  worktree. Don't revive a session that `agp bg --json` shows as `moving`.
- Don't pin lanes with `--as` to spread load. agp does that.

## Inside an agp session

`AGP=1`, `AGP_PROFILE` (account id) and `AGP_LABEL` are set. A nested
`agp -p` or `agp --bg` works as usual. A nested interactive `agp` starts
the plain tool.
