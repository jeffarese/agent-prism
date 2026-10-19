#!/usr/bin/env python3
"""Builds a fake home with fictional accounts for the README demo.

Usage: demo/setup.py <root>

Everything goes under <root>: a fake HOME, agent-prism config and state, a
usage cache with fictional numbers, and a fake `claude` on PATH. Nothing
talks to Anthropic or OpenAI, and the real Keychain is never read
(AGP_SECRET_STORE_FILE points at an empty map, AGP_NO_POLL=1).

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

# id, label, dir, plan, tier, name, email, org, 5h %, 5h reset, 7d %, 7d reset
CLAUDE = [
    ("work", "Work", ".claude-work", "team", "default_claude_max_5x",
     "Ana Lima", "ana@acme.example", "Acme Inc", 58.0, 1 * HOUR + 12 * 60, 30.0, 14 * HOUR),
    ("work2", "Work 2", ".claude-work2", "team", "default_claude_max_5x",
     "Ana Lima", "ana+2@acme.example", "Acme Inc", 9.0, 4 * HOUR + 40 * 60, 52.0, 5 * DAY + 2 * HOUR),
    ("personal", "Personal", ".claude-personal", "max", "default_claude_max_5x",
     "Ana Lima", "ana@lima.example", None, 23.0, 3 * HOUR + 5 * 60, 41.0, 4 * DAY + 3 * HOUR),
]
# id, label, dir, plan, email, 5h %, 5h reset, 7d %, 7d reset
CODEX = [
    ("plus", "ChatGPT Plus", ".codex-plus", "plus", "ana@lima.example",
     12.0, 1 * HOUR + 50 * 60, 34.0, 6 * DAY),
]


def b64(data: bytes) -> str:
    return base64.urlsafe_b64encode(data).decode().rstrip("=")


def jwt(claims: dict) -> str:
    head = b64(json.dumps({"alg": "none"}).encode())
    return f"{head}.{b64(json.dumps(claims).encode())}.demo"


def link(target: Path, at: Path) -> None:
    at.symlink_to(target)


def window(util: float, reset_in: int) -> dict:
    return {"utilization": util, "resets_at": NOW + reset_in}


def cache(util5: float, reset5: int, util7: float, reset7: int, expires: int) -> dict:
    return {
        "snapshot": {
            "fetched_at": NOW - 25,
            "five_hour": window(util5, reset5),
            "seven_day": window(util7, reset7),
        },
        "last_attempt": NOW - 25,
        "last_error": None,
        "backoff_until": None,
        "token_expires_at": expires,
    }


def main() -> None:
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    root = Path(sys.argv[1]).resolve()
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
    codex_common = home / ".codex"
    (codex_common / "sessions").mkdir(parents=True)
    (codex_common / "config.toml").write_text("")

    expires = NOW + 6 * HOUR
    profiles = []
    for (pid, label, dirname, plan, tier, name, email, org,
         u5, r5, u7, r7) in CLAUDE:
        d = home / dirname
        d.mkdir()
        for entry in ("projects", "settings.json", "CLAUDE.md"):
            link(common / entry, d / entry)
        account = {"displayName": name, "emailAddress": email}
        if org:
            account["organizationName"] = org
        (d / ".claude.json").write_text(json.dumps({"oauthAccount": account}))
        creds = {"claudeAiOauth": {
            "accessToken": "demo-not-a-token",
            "refreshToken": "demo-not-a-token",
            "expiresAt": expires * 1000,
            "scopes": ["user:inference"],
            "subscriptionType": plan,
            "rateLimitTier": tier,
        }}
        (d / ".credentials.json").write_text(json.dumps(creds))
        (usage / f"{pid}.json").write_text(json.dumps(cache(u5, r5, u7, r7, expires)))
        profiles.append(f'[[profile]]\nid = "{pid}"\nlabel = "{label}"\ndir = "~/{dirname}"\n')

    for (pid, label, dirname, plan, email, u5, r5, u7, r7) in CODEX:
        d = home / dirname
        d.mkdir()
        for entry in ("sessions", "config.toml"):
            link(codex_common / entry, d / entry)
        id_token = jwt({
            "email": email,
            "name": "Ana Lima",
            "https://api.openai.com/auth": {
                "chatgpt_plan_type": plan,
                "chatgpt_account_id": f"demo-{pid}",
            },
        })
        auth = {
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": None,
            "tokens": {
                "id_token": id_token,
                "access_token": jwt({"exp": expires}),
                "refresh_token": "demo-not-a-token",
                "account_id": f"demo-{pid}",
            },
        }
        (d / "auth.json").write_text(json.dumps(auth))
        (usage / f"{pid}.json").write_text(json.dumps(cache(u5, r5, u7, r7, expires)))
        profiles.append(
            f'[[profile]]\nid = "{pid}"\nlabel = "{label}"\ndir = "~/{dirname}"\ntool = "codex"\n'
        )

    fake = bin_dir / "claude"
    shutil.copy(HERE / "fake-claude.py", fake)
    fake.chmod(0o755)

    (config_dir / "config.toml").write_text(f"""common_home = "~/.claude"

[claude]
bin = "{fake}"

[balance]
policy = "smart"
on_limit = "auto"
countdown_secs = 4

[dock]
mode = "full"
icons = "unicode"

""" + "\n".join(profiles))

    (root / "secrets.json").write_text("{}")
    agp_bin = Path(os.environ.get("AGP_BIN_DIR", HERE.parent / "target" / "release"))
    (root / "env.sh").write_text(f"""export HOME="{home}"
export AGP_CONFIG_DIR="{config_dir}"
export AGP_STATE_DIR="{state}"
export AGP_SECRET_STORE_FILE="{root / 'secrets.json'}"
export AGP_NO_POLL=1
export PATH="{agp_bin}:{bin_dir}:/usr/bin:/bin"
mkdir -p "$HOME/code/weather-app" && cd "$HOME/code/weather-app"
""")
    print(root / "env.sh")


if __name__ == "__main__":
    main()
