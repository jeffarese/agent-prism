# Background sessions with agp (`claude --bg`)

`agp` in front of a `claude --bg` line starts the background session on
the best account. It then leaves a small watcher that handles the
session's limits. Claude Code alone leaves a limited session blocked until
someone acts.

## Start

```sh
agp --bg --name fix-auth "fix the auth bug"
agp --on-limit wait --max-wait 6h -- --bg --name nightly "run the migration"
agp --bg --resume <session id> "go on"     # on the account that last ran it
```

The launch is a drop-in: same args, stdin, stdout, stderr and exit code.
agp exits as soon as `claude --bg` does. Background lanes are spread over
the accounts like every other run (see [lanes.md](lanes.md)).

What the watcher does on a limit:

| `--on-limit` | Watcher |
| --- | --- |
| `switch` (default) | Stops the session and resumes the same conversation on the best other account. No free account: waits. |
| `wait` | Waits for the reset (at most `--max-wait`), then wakes the session on the same account. |
| `stay`, `fail` | No watcher. |

## Watch and move

```sh
agp bg                        # sessions agp watches: account, state, moves
agp bg --json                 # the same for scripts
agp bg watch <id>             # watch a session agp did not start
agp bg move <id> --to <account>   # move one now (--json for a result)
```

`agp bg --json` prints `{"sessions": [...]}`, newest change first. Useful
keys: `session_id`, `short_id`, `name`, `profile`, `label`, `state`,
`limit` (`text`, `resets_at`), `moves`, `watching`.

`state` is one of:

- `watching`: running; the watcher looks for limits.
- `waiting`: limited, waiting for the reset (`wait_until`).
- `moving`: being stopped and resumed elsewhere. Don't touch it.
- `limited`: limited, and the watcher will not act (`stay`, or past
  `--max-wait`).
- `ended`: the job is gone. `dormant`: no change for 7 days; `agp bg watch`
  re-arms it.

## Rules

- Never `claude rm` a session agp moved. The old account keeps the
  stopped job because the session still uses its worktree.
- Don't wake, respawn or resume a session while `agp bg --json` shows it
  as `moving`.
- Before `claude respawn <id>`, run `agp bg watch <id>`. It rewrites the
  `--settings` file agp passes, so the respawn gets your new settings, and
  it re-arms the watcher.
- After a move the session runs under the same session id on another
  account. Its old background tasks and subagents stopped; it starts again
  the ones it still needs.
- Moves need shared `projects/` between accounts and a trusted folder. If
  `agp bg move` fails for that reason, tell the user (`agp doctor`); don't
  run `agp adopt --apply` yourself.
