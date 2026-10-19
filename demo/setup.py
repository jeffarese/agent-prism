#!/usr/bin/env python3
"""Builds a fake home with fictional accounts for the README demo.

Usage: demo/setup.py [--onboarding] <root>

Everything goes under <root>: a fake HOME, agent-prism config and state, a
usage cache with fictional numbers, and a fake `claude` on PATH. Nothing
talks to Anthropic or OpenAI, and the real Keychain is never read
(AGP_SECRET_STORE_FILE points at an empty map, AGP_NO_POLL=1).

With --onboarding the config has no accounts, so `agp` starts the
onboarding wizard and finds the fictional accounts itself.

Writes <root>/env.sh; source it to run `agp` against the fake home.
"""

import base64
import json
import os
import shutil
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
NOW = int(time.time())
HOUR = 3600
DAY = 24 * HOUR
NAME = "Ana Lima"

# One person with their own accounts. The default install (~/.claude) is the
# personal one. Fields: id, label, dir, plan, tier, email, org,
# 5h %, 5h reset, 7d %, 7d reset.
CLAUDE = [
    ("personal", "Personal", ".claude", "max", "default_claude_max_5x",
     "ana@lima.example", None, 23.0, 3 * HOUR + 5 * 60, 41.0, 4 * DAY + 3 * HOUR),
    ("work", "Work", ".claude-work", "team", "default_claude_max_5x",
     "ana@acme.example", "Acme Inc", 58.0, 1 * HOUR + 12 * 60, 30.0, 14 * HOUR),
    ("side", "Side project", ".claude-side", "pro", "default_claude_pro",
     "ana@studio.example", None, 6.0, 4 * HOUR + 40 * 60, 12.0, 6 * DAY),
]
# The default Codex install (~/.codex) with a ChatGPT Plus login.
CODEX = ("chatgpt", "ChatGPT", ".codex", "plus", "ana@lima.example",
         12.0, 1 * HOUR + 50 * 60, 34.0, 6 * DAY)


def b64(data: bytes) -> str:
    return base64.urlsafe_b64encode(data).decode().rstrip("=")


def jwt(claims: dict) -> str:
    head = b64(json.dumps({"alg": "none"}).encode())
    return f"{head}.{b64(json.dumps(claims).encode())}.demo"


def window(util: float, reset_in: int) -> dict:
    return {"utilization": util, "resets_at": NOW + reset_in}


def cache(util5: float, reset5: int, util7: float, reset7: int, expires: int) -> str:
    return json.dumps({
        "snapshot": {
            "fetched_at": NOW - 25,
            "five_hour": window(util5, reset5),
            "seven_day": window(util7, reset7),
        },
        "last_attempt": NOW - 25,
        "last_error": None,
        "backoff_until": None,
        "token_expires_at": expires,
    })


def wizard_id(dirname: str) -> str:
    """The id that the onboarding wizard suggests for a dir."""
    if dirname in (".claude", ".codex"):
        return "default" if dirname == ".claude" else "codex"
    return dirname.removeprefix(".claude-")


def main() -> None:
    args = sys.argv[1:]
    onboarding = "--onboarding" in args
    args = [a for a in args if a != "--onboarding"]
    if len(args) != 1:
        sys.exit(__doc__)
    root = Path(args[0]).resolve()
    if root.exists():
        shutil.rmtree(root)
    home = root / "home"
    config_dir = root / "config"
    state = root / "state"
    usage = state / "usage"
    bin_dir = root / "bin"
    for d in (home, config_dir, usage, bin_dir):
        d.mkdir(parents=True)

    # The common homes that all accounts share.
    common = home / ".claude"
    (common / "projects").mkdir(parents=True)
    (common / "settings.json").write_text("{}\n")
    (common / "CLAUDE.md").write_text("# Notes for Claude\n")

    expires = NOW + 6 * HOUR
    profiles = []
    for (pid, label, dirname, plan, tier, email, org, u5, r5, u7, r7) in CLAUDE:
        d = home / dirname
        d.mkdir(exist_ok=True)
        if d != common:
            for entry in ("projects", "settings.json", "CLAUDE.md"):
                (d / entry).symlink_to(common / entry)
        account = {"displayName": NAME, "emailAddress": email}
        if org:
            account["organizationName"] = org
        # The default install keeps its account file next to the dir.
        account_file = home / ".claude.json" if d == common else d / ".claude.json"
        account_file.write_text(json.dumps({"oauthAccount": account}))
        creds = {"claudeAiOauth": {
            "accessToken": "demo-not-a-token",
            "refreshToken": "demo-not-a-token",
            "expiresAt": expires * 1000,
            "scopes": ["user:inference"],
            "subscriptionType": plan,
            "rateLimitTier": tier,
        }}
        (d / ".credentials.json").write_text(json.dumps(creds))
        for i in {pid, wizard_id(dirname)}:
            (usage / f"{i}.json").write_text(cache(u5, r5, u7, r7, expires))
        profiles.append(f'[[profile]]\nid = "{pid}"\nlabel = "{label}"\ndir = "~/{dirname}"\n')

    pid, label, dirname, plan, email, u5, r5, u7, r7 = CODEX
    d = home / dirname
    (d / "sessions").mkdir(parents=True)
    (d / "config.toml").write_text("")
    id_token = jwt({
        "email": email,
        "name": NAME,
        "https://api.openai.com/auth": {
            "chatgpt_plan_type": plan,
            "chatgpt_account_id": f"demo-{pid}",
        },
    })
    (d / "auth.json").write_text(json.dumps({
        "auth_mode": "chatgpt",
        "OPENAI_API_KEY": None,
        "tokens": {
            "id_token": id_token,
            "access_token": jwt({"exp": expires}),
            "refresh_token": "demo-not-a-token",
            "account_id": f"demo-{pid}",
        },
    }))
    for i in {pid, wizard_id(dirname)}:
        (usage / f"{i}.json").write_text(cache(u5, r5, u7, r7, expires))
    profiles.append(
        f'[[profile]]\nid = "{pid}"\nlabel = "{label}"\ndir = "~/{dirname}"\ntool = "codex"\n'
    )

    fake = bin_dir / "claude"
    shutil.copy(HERE / "fake-claude.py", fake)
    fake.chmod(0o755)

    config = f"""common_home = "~/.claude"

[balance]
countdown_secs = 4

[dock]
mode = "full"
icons = "unicode"
"""
    if not onboarding:
        config += "\n" + "\n".join(profiles)
    (config_dir / "config.toml").write_text(config)

    (root / "secrets.json").write_text("{}")
    agp_bin = Path(os.environ.get("AGP_BIN_DIR", HERE.parent / "target" / "release"))
    (root / "env.sh").write_text(f"""export HOME="{home}"
export AGP_CONFIG_DIR="{config_dir}"
export AGP_STATE_DIR="{state}"
export AGP_SECRET_STORE_FILE="{root / 'secrets.json'}"
export AGP_NO_POLL=1
export DEMO_EVENTS="{root / 'events.log'}"
export PATH="{agp_bin}:{bin_dir}:/usr/bin:/bin"
mkdir -p "$HOME/code/weather-app" && cd "$HOME/code/weather-app"
""")
    print(root / "env.sh")


if __name__ == "__main__":
    main()
