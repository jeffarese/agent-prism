# /// script
# requires-python = ">=3.10"
# dependencies = ["pillow"]
# ///
"""Adds a caption bar to the demo video and makes the README GIF.

Usage: uv run demo/captions.py <video.mp4> <events.log> <out.gif>

events.log has lines "<name> <unix time>" from demo/fake-claude.py and the
tape ("start" is the first visible frame). Each caption shows from one
event to the next.
"""

import subprocess
import sys
import tempfile
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

BAR_H = 64
BG = "#11111b"
TEXT = "#cdd6f4"
ACCENT = "#cba6f7"
FONT = "/System/Library/Fonts/SFNS.ttf"
FPS = 15
# Show a caption a little before its event: the video runs slightly
# behind the event clock, and readers need a moment to read.
LEAD = 0.4

# (from event, caption). A caption ends where the next one starts.
CAPTIONS = [
    ("start", "agp starts Claude Code on the account with the most quota left"),
    ("prompt", "You work as usual. The dock shows the usage of each account"),
    ("limit", "Work hits its 5-hour limit. agp counts down, then switches"),
    ("resume", "The same conversation continues on Personal"),
    ("done", "The task is done. Same session, another account"),
]


def probe(video: Path, entry: str) -> str:
    return subprocess.check_output([
        "ffprobe", "-v", "error", "-select_streams", "v:0",
        "-show_entries", entry, "-of", "csv=p=0", str(video),
    ]).decode().strip()


def bar(width: int, step: int, total: int, text: str, path: Path) -> None:
    img = Image.new("RGB", (width, BAR_H), BG)
    d = ImageDraw.Draw(img)
    big = ImageFont.truetype(FONT, 25)
    small = ImageFont.truetype(FONT, 19)
    pill = f"{step}/{total}"
    pw = d.textlength(pill, font=small) + 24
    x, y = 24, (BAR_H - 34) // 2
    d.rounded_rectangle((x, y, x + pw, y + 34), radius=17, fill=ACCENT)
    d.text((x + pw / 2, y + 17), pill, font=small, fill=BG, anchor="mm")
    d.text((x + pw + 18, BAR_H / 2), text, font=big, fill=TEXT, anchor="lm")
    img.save(path)


def main() -> None:
    if len(sys.argv) != 4:
        sys.exit(__doc__)
    video, events_path, out = (Path(a) for a in sys.argv[1:])
    events = {}
    for line in events_path.read_text().splitlines():
        name, t = line.split()
        events.setdefault(name, float(t))
    t0 = events["start"]
    width = int(probe(video, "stream=width"))
    duration = float(probe(video, "format=duration"))
    times = [max(events[name] - t0 - LEAD, 0) for name, _ in CAPTIONS] + [duration + 1]

    with tempfile.TemporaryDirectory() as tmp:
        inputs, chain = ["-i", str(video)], f"[0:v]pad=iw:ih+{BAR_H}:0:{BAR_H}:color={BG}[v0]"
        for i, (_, text) in enumerate(CAPTIONS):
            png = Path(tmp) / f"c{i}.png"
            bar(width, i + 1, len(CAPTIONS), text, png)
            inputs += ["-i", str(png)]
            start, end = times[i], times[i + 1]
            chain += (f";[v{i}][{i + 1}:v]overlay=0:0:"
                      f"enable='between(t,{start:.2f},{end:.2f})'[v{i + 1}]")
        last = f"v{len(CAPTIONS)}"
        chain += (f";[{last}]fps={FPS},split[a][b];[a]palettegen=stats_mode=diff[p];"
                  f"[b][p]paletteuse=dither=none")
        subprocess.run(["ffmpeg", "-v", "error", "-y", *inputs,
                        "-filter_complex", chain, str(out)], check=True)
    print(out)


if __name__ == "__main__":
    main()
