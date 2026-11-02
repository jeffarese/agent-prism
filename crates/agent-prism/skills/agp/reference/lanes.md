# Parallel lanes with agp

A lane is one agent run: `agp -p`, `agp codex exec`, `agp --bg`, or an
interactive `agp` / `agp codex` in a terminal pane.

## How agp spreads lanes

- Every run agp starts holds a claim on its account while it runs. That
  covers every mode: interactive `agp` and `agp codex`, `agp -p`,
  `agp -p --on-limit ...`, `agp codex exec ...`, `agp --bg ...`, and runs
  pinned with `--as`.
- Picks are serialized. Lanes started in the same second see each other,
  so a `for ... &` loop spreads fine.
- Each live run counts as expected 5h usage on its account, on top of the
  measured usage and the account's burn rate over the last hour. The
  default is `balance.run_load = 25`: 25% of a 5h window per working
  run-hour on a plan of capacity 1.0 (Max 5x, team), half that on 20x. An
  interactive session waiting for its user counts a quarter.
- Moves on a limit are spread the same way, so lanes that hit limits
  together don't all land on one account.

So: start lanes without `--as` and let agp place them.

## Before a fan-out

```sh
agp status --json | jq .capacity          # {"claude": 5, "codex": 2}
agp pick --n 8                            # where 8 new Claude runs would go
agp pick --n 3 --codex --json | jq -r '.plan[] | "\(.label)\t\(.reason)"'
```

- `capacity.claude` / `capacity.codex`: how many more runs the pool can
  take now before every account would reach its soft 5h mark
  (`balance.soft_5h`).
- agp weighs each lane by past runs like it (same repo, similar first
  prompt): a lane whose earlier copies burned 2x counts as 2 runs, so heavy
  lanes spread on their own. `agp learn --prompt "<lane prompt>" --json`
  shows the weight (`new_run`). `capacity` and `agp pick --n` count runs of
  weight 1, so with heavy lanes (weight 2) plan about half of `capacity`.
- `agp pick --n <k>`: one line per run, best first. Nothing is claimed, so
  the real lanes may land a bit differently. JSON:
  `{"chosen", "reason", "earliest_reset", "plan": [{"profile", "label", "reason"}]}`.

If the plan needs more lanes than `capacity`:

1. Tell the user: "The accounts have room for 5 more Claude runs now; 12
   lanes would share accounts and hit the 5h limit sooner."
2. Offer a choice: run in batches of `capacity`, run all with
   `--on-limit wait` (slower, finishes), or run all anyway.

Don't overload the pool silently.

## Batch patterns

Print-mode lanes that must finish:

```sh
for f in specs/*.md; do
  agp --on-limit switch --max-wait 3h -p "implement $(basename "$f")" \
    --output-format json > "out/$(basename "$f" .md).json" &
done
wait
```

In batches of the current capacity:

```sh
n=$(agp status --json | jq '.capacity.claude')
[ "$n" -lt 1 ] && n=1
ls tasks/*.md | xargs -P "$n" -I{} agp --on-limit switch -p "do {}"
```

Codex lanes:

```sh
agp codex --on-limit switch exec "fix the lint errors in src/api" &
agp codex --on-limit switch exec "fix the lint errors in src/ui" &
wait
```

Check results by exit code: `0` done, `75` gave up on a limit (retry
later), anything else is the tool's own failure.

## Interactive lanes in panes (Rex, herdr, tmux)

Start each pane with a compact dock. The lane moves itself to another
account on a limit, like a normal `agp` session.

```sh
agp --dock compact -- --model sonnet "fix the flaky test in auth"
agp codex --dock compact -- "update the changelog"
```

Everything after `--` goes to `claude` (or `codex`) unchanged; the prompt
comes last.

If a pane prints `agp: already inside an agent-prism session; starting
plain claude`, it inherited `AGP_SOCK` from your own agp session, so it
has no dock and won't move on a limit. Start it with
`env -u AGP_SOCK agp --dock compact -- ...`.

## Long lanes

- `agp --bg ...`: a Claude Code background session. A watcher moves it or
  waits for the reset. See [background.md](background.md).
- `agp --on-limit switch -p ...`: agp stays the parent and resumes the
  conversation on another account.
- `agp --on-limit wait --max-wait <dur> -p ...`: stays on the account and
  waits.

## Follow-ups on the same account

Resume a lane where it ran:

```sh
sid=$(agp -p "review src/api" --output-format json | jq -r .session_id)
agp -p --resume "$sid" "fix what you found"
agp --bg --resume "$sid" "fix what you found"
agp codex exec resume <thread id> "fix what you found"
```

agp starts a resume (`agp -p --resume`, `agp --resume`, `agp --bg --resume`,
`agp codex exec resume`, `agp codex resume`) on the account that last ran
that session, when it is not limited and has room; else on the best other
account. `-c`/`--continue` uses the last session agp ran in this folder.
`--as` still wins.

## When to use `--as`

Only when the user names the account ("run it on work"). Never to spread
load, and never to pick the account that looks emptiest in `agp status`:
that view does not show the lanes you are about to start, and agp does.
