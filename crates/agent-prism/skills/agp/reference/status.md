# Reading agp status, and troubleshooting

## `agp status`

```sh
agp status            # a table per tool (Claude, Codex)
agp status --json     # the same as JSON
agp status --watch    # refresh every 30 s
```

Columns: account, email, plan, 5H use and reset, 7D use and reset, scoped
(per-model weekly) use, RUNS (live agp runs on the account), STATE, POOL.
`▸` marks the account the next new run would use; `next:` says why.
A `~` before a percent means agp inferred it (a past reset means 0%).

| STATE | Meaning | What to do |
| --- | --- | --- |
| `fresh` | Usage just read. | Nothing. |
| `stale` | Usage is a bit old. | Nothing; agp refreshes it. |
| `no data` | agp could not read usage now. | Nothing; runs still work. |
| `idle` | agp's copy of the access token ran out because nothing used the account for hours. Older agp versions said `expired`. | Nothing. Still logged in and usable; Claude refreshes the token on the next run there. Don't ask the user to log in, and don't avoid the account. |
| `limited` | At a 5h, weekly or model limit. | Nothing; agp skips it until the reset. |
| `no login` | No login for this account. | Tell the user to log in: `agp add <id>`, or `/login` in a session on that account (`agp --as <id>`). |

## `agp status --json`

```json
{
  "views": [{
    "profile_id": "work", "label": "Work",
    "identity": {"email": "…", "subscription": "max", "tool": "claude"},
    "token": {"Valid": {"expires_at": 1793500000}},
    "usage": {"five_hour": {"utilization": 42.0, "resets_at": 1793510000},
              "seven_day": {"utilization": 18.0, "resets_at": 1793900000},
              "scoped": []},
    "limited": null, "in_pool": true, "next_pick": true, "reason": "…",
    "runs": 2, "load_5h": 71.5, "usable": true
  }],
  "pick": {"chosen": "work", "reason": "…", "earliest_reset": null},
  "picks": {"claude": {…}, "codex": {…}},
  "capacity": {"claude": 3, "codex": 1}
}
```

- `usable`: in the pool, logged in and not limited. An account whose token
  is only idle (expired copy) is still usable. Use this, not `token`.
- `runs`: live agp runs on the account, all modes.
- `load_5h`: expected 5h use in percent one hour from now, counting the
  runs on it. `null` when unknown.
- `limited`: `{"kind": "FiveHour" | "SevenDay" | "Scoped", "until": <unix s>}`
  or `null`.
- `token` keeps its old shape (`Valid`, `Expired`, `Missing`). `Expired`
  is the `idle` state, not a logout.
- `capacity`: how many more runs of each tool the pool can take now before
  every account would reach its soft 5h mark.
- `pick` is the Claude pick; `picks` has one per tool. Times are unix
  seconds.

Handy queries:

```sh
agp status --json | jq .capacity
agp status --json | jq -r '.views[] | select(.usable) | "\(.label)\truns=\(.runs)\tload=\(.load_5h)"'
agp status --json | jq -r '.views[] | select(.token == "Missing") | .label'   # need a login
```

## `agp pick`

```sh
agp pick                      # the account a new Claude run would use
agp pick --codex --json       # the same for Codex
agp pick --n 4 --json         # where 4 new runs would go
```

`--n <k>` prints one line per run. JSON adds `"plan": [{"profile",
"label", "reason"}]` next to `chosen`, `reason` and `earliest_reset`.
Nothing is claimed: real runs claim their account when they start.

## Troubleshooting

- Exit 75: agp gave up on a limit (`--max-wait` passed, or
  `--on-limit fail` with every usable account limited). Retry after
  `earliest_reset` from `agp pick --json`.
- A run fails with an authentication error (401, "Please run /login"):
  that account needs a login. Tell the user which one
  (`AGP_PROFILE`/`AGP_LABEL` inside the run, or the account in
  `agp status`). Rerun the task without `--as` meanwhile; agp picks
  another account.
- `agp: no config ... run agp setup`: agp is not set up. Tell the user to
  run `agp setup`. Don't run it yourself.
- agp can't pick an account (no accounts, all limited) without
  `--on-limit`: it writes one line to stderr and runs the tool anyway.
- Many lanes hit limits together: you started more than `capacity`. Run
  fewer at once, or use `--on-limit wait`.
- A pane lane says `already inside an agent-prism session; starting plain
  claude`: see [lanes.md](lanes.md#interactive-lanes-in-panes-rex-herdr-tmux).
- `agp doctor` checks the setup and suggests fixes. It is safe to run.
- `agp dashboard` shows usage, history and runs in the browser; it keeps
  running until Ctrl+C, so don't start it from a script.
