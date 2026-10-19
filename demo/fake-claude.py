#!/usr/bin/env python3
"""A stand-in for `claude` in the README demo. Never talks to Anthropic.

It looks a little like Claude Code, runs agent-prism's hooks from the
--settings file, and acts out one task: the first turn stops at the 5-hour
limit (it sets the active account's cached 5h usage to 100% and sends
StopFailure rate_limit). After agent-prism resumes the session on another
account, the turn completes.
"""

import json
import os
import subprocess
import sys
import time
from pathlib import Path

VALUE_FLAGS = {"--settings", "--session-id", "--resume", "--model", "--append-system-prompt",
               "--permission-mode", "--add-dir", "--name", "--mcp-config", "--agent"}

R = "\033[0m"
BOLD = "\033[1m"
DIM = "\033[2m"
RED = "\033[31m"
GREEN = "\033[32m"
ORANGE = "\033[38;5;173m"


def parse(argv):
    opts, prompt = {}, None
    i = 0
    while i < len(argv):
        a = argv[i]
        if a in VALUE_FLAGS and i + 1 < len(argv):
            opts[a] = argv[i + 1]
            i += 2
            continue
        if not a.startswith("-"):
            prompt = a
        i += 1
    return opts, prompt


def hook_commands(settings_path):
    try:
        data = json.loads(Path(settings_path).read_text())
    except (OSError, ValueError, TypeError):
        return {}
    found = {}

    def walk(v):
        if isinstance(v, dict):
            for x in v.values():
                walk(x)
        elif isinstance(v, list):
            for x in v:
                walk(x)
        elif isinstance(v, str) and " hook " in v:
            found[v.rsplit(" ", 1)[-1]] = v

    walk(data)
    return found


class Fake:
    def __init__(self):
        self.opts, self.prompt = parse(sys.argv[1:])
        self.hooks = hook_commands(self.opts.get("--settings"))
        self.resumed = "--resume" in self.opts
        self.sid = self.opts.get("--resume") or self.opts.get("--session-id") or "demo-session"
        home = Path(os.environ["HOME"])
        self.transcript = home / ".claude" / "projects" / f"{self.sid}.log"
        # The default install (no CLAUDE_CONFIG_DIR) keeps it in ~/.claude.json.
        cfg = os.environ.get("CLAUDE_CONFIG_DIR")
        account_file = Path(cfg) / ".claude.json" if cfg else home / ".claude.json"
        try:
            acct = json.loads(account_file.read_text())["oauthAccount"]
        except (OSError, ValueError, KeyError):
            acct = {}
        self.who = " · ".join(x for x in (acct.get("emailAddress"), acct.get("organizationName")) if x)

    def event(self, name):
        """Logs a story event for the captions (demo/captions.py)."""
        path = os.environ.get("DEMO_EVENTS")
        if path:
            with open(path, "a") as f:
                f.write(f"{name} {time.time():.3f}\n")

    def hook(self, event, **payload):
        cmd = self.hooks.get(event)
        if not cmd:
            return
        payload = {"session_id": self.sid, "cwd": os.getcwd(), "hook_event_name": event, **payload}
        subprocess.run(cmd, shell=True, input=json.dumps(payload).encode(), check=False)

    def out(self, text="", delay=0.0, keep=True):
        print(text, flush=True)
        if keep:
            with self.transcript.open("a") as f:
                f.write(text + "\n")
        time.sleep(delay)

    def think(self, secs):
        frames = "·✢✳✶✻✽"
        end = time.time() + secs
        i = 0
        while time.time() < end:
            sys.stdout.write(f"\r{ORANGE}{frames[i % len(frames)]}{R} {DIM}Thinking…{R}")
            sys.stdout.flush()
            time.sleep(0.12)
            i += 1
        sys.stdout.write("\r\033[K")
        sys.stdout.flush()

    def banner(self):
        print(f"{ORANGE}✻{R} {BOLD}Claude Code{R} {DIM}(demo stand-in){R}")
        print(f"  {DIM}~/code/weather-app · {self.who}{R}")
        print()

    def limit(self):
        profile = os.environ.get("AGP_PROFILE", "")
        cache = Path(os.environ["AGP_STATE_DIR"]) / "usage" / f"{profile}.json"
        try:
            data = json.loads(cache.read_text())
            data["snapshot"]["five_hour"]["utilization"] = 100.0
            data["snapshot"]["fetched_at"] = int(time.time())
            cache.write_text(json.dumps(data))
        except (OSError, ValueError, KeyError, TypeError):
            pass
        self.event("limit")
        self.out(f"  {RED}⎿  5-hour limit reached · resets 4:20pm{R}", keep=False)
        self.hook("StopFailure", error="rate_limit")

    def first_turn(self, text):
        self.event("prompt")
        self.out(f"{BOLD}❯{R} {text}")
        self.out()
        self.hook("UserPromptSubmit", prompt=text)
        self.think(1.2)
        self.out(f"{BOLD}⏺{R} I'll add retries with exponential backoff to the API client.", 0.5)
        self.out()
        self.out(f"{GREEN}⏺{R} {BOLD}Read{R}(src/api/client.ts)")
        self.out(f"  {DIM}⎿  Read 84 lines{R}", 0.6)
        self.out()
        self.out(f"{GREEN}⏺{R} {BOLD}Update{R}(src/api/client.ts)")
        self.out(f"  {DIM}⎿  Added 18 lines{R}", 0.8)
        self.out()
        self.think(0.8)
        self.limit()

    def resumed_turn(self, text):
        self.out(f"{BOLD}❯{R} {text}", keep=False)
        print()
        self.hook("UserPromptSubmit", prompt=text)
        self.think(1.0)
        self.out(f"{BOLD}⏺{R} Continuing where I stopped: the tests are next.", 0.5)
        self.out()
        self.out(f"{GREEN}⏺{R} {BOLD}Update{R}(src/api/client.test.ts)")
        self.out(f"  {DIM}⎿  Added 31 lines{R}", 0.6)
        self.out()
        self.out(f"{GREEN}⏺{R} {BOLD}Bash{R}(npm test)")
        self.think(1.0)
        self.out(f"  {DIM}⎿  {GREEN}✓{R}{DIM} 42 passed{R}", 0.5)
        self.out()
        self.out(f"{BOLD}⏺{R} Done. fetchJson() now retries 3 times (250 ms, 500 ms, 1 s).")
        self.out()
        self.event("done")
        self.hook("Stop")

    def run(self):
        self.banner()
        if self.resumed:
            self.event("resume")
            print(f"{DIM}  ↻ resumed session {self.sid[:8]}{R}")
            print()
            if self.transcript.exists():
                for line in self.transcript.read_text().splitlines():
                    print(line)
            self.hook("SessionStart", source="resume")
            if self.prompt:
                self.resumed_turn(self.prompt)
        else:
            self.transcript.unlink(missing_ok=True)
            self.hook("SessionStart", source="startup")
        while True:
            try:
                line = input(f"{BOLD}❯{R} ")
            except (EOFError, KeyboardInterrupt):
                break
            if line.strip() in ("/exit", "exit"):
                break
            # Move up over the echoed input and print it as part of the transcript.
            sys.stdout.write("\033[1A\033[K")
            if line.strip():
                self.first_turn(line.strip())
                # The limit stopped the turn: wait for agent-prism to switch.
                while True:
                    time.sleep(60)
        self.hook("SessionEnd", reason="exit")


if __name__ == "__main__":
    Fake().run()
