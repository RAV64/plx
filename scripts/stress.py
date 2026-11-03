#!/usr/bin/env python3
"""Floods the terminal with heavy, varied output and reports how fast it went.

    python3 scripts/stress.py [MiB] [scroll|redraw] [no-links] [no-truecolor]

scroll (default): log-like lines through the history, every line different.
They carry truecolor and 256-colour text, bold, italic, curly and coloured
underlines, strikethrough and inverse; wide CJK characters; emoji with skin
tones, flags and ZWJ families; combining accents; Arabic and Hebrew; box
drawing; links (OSC 8); `\\r` progress-bar overwrites; and lines longer than
the screen, so they wrap.

redraw: full-screen frames drawn over each other in place, as htop or a
game would. It reports frames per second too.

`no-links` leaves out the links, `no-truecolor` the random 24-bit colours:
every distinct link and colour takes room in the terminal's page tables,
which libghostty grows by copying the page.

The output is built in memory before the clock starts, so only the writing
is timed. The time ends when the terminal has taken the last byte, which is
close to when it is drawn, but not exactly.
"""

import os
import random
import shutil
import sys
import time

ESC = "\x1b"

WIDE = "漢字仮名交じり文한국어中文字符表示テスト"
EMOJI = [
    "😀", "🚀", "🔥", "✨", "🎉", "👍🏽", "👩🏻‍💻", "👨‍👩‍👧‍👦", "🏳️‍🌈", "🇫🇮", "🇯🇵", "❤️", "🧑🏿‍🚀",
]
COMBINING = ["é", "ñ", "ü", "å", "ẽ", "o̵", "a͂", "Z̤͔ͧ̑̓ä͖̭̈̇lͮ̒ͫǧ̗͚̚o̙̔ͮ̇͐"]
SCRIPTS = ["مرحبا بالعالم", "שלום עולם", "Привет, мир", "Γειά σου κόσμε", "नमस्ते दुनिया"]
BOX = "─│┌┐└┘├┤┬┴┼═║╔╗╚╝╠╣╦╩╬░▒▓█▀▄"
SYMBOLS = "∀∂∈ℝ∑∏√∞≈≠≤≥⊂⊃∧∨⇒⇔→←↑↓♠♣♥♦★☆♪♫"
WORDS = (
    "request handled cache miss retrying upstream connection pool exhausted "
    "compiled module linking target warning deprecated field timeout parsed "
    "token stream allocation freed scheduler tick queue drained flushed"
).split()
LEVELS = [("INFO", 32), ("WARN", 33), ("ERROR", 31), ("DEBUG", 36), ("TRACE", 35)]


def truecolor(rng, text):
    r, g, b = rng.randrange(256), rng.randrange(256), rng.randrange(256)
    return f"{ESC}[38;2;{r};{g};{b}m{text}{ESC}[39m"


def palette(rng, text):
    return f"{ESC}[38;5;{rng.randrange(256)}m{text}{ESC}[39m"


def styled(rng, text):
    style = rng.choice([
        "1", "3", "4", "4:3", "9", "7", "2", "1;3", "4:3;58;2;255;80;80", "53",
    ])
    return f"{ESC}[{style}m{text}{ESC}[0m"


def link(rng, text):
    return f"{ESC}]8;;https://example.com/{rng.randrange(10**6)}{ESC}\\{text}{ESC}]8;;{ESC}\\"


def piece(rng):
    kind = rng.randrange(10)
    if kind == 0:
        return "".join(rng.choice(WIDE) for _ in range(rng.randrange(2, 8)))
    if kind == 1:
        return "".join(rng.choice(EMOJI) for _ in range(rng.randrange(1, 4)))
    if kind == 2:
        return rng.choice(COMBINING)
    if kind == 3:
        return rng.choice(SCRIPTS)
    if kind == 4:
        return "".join(rng.choice(BOX) for _ in range(rng.randrange(4, 20)))
    if kind == 5:
        return "".join(rng.choice(SYMBOLS) for _ in range(rng.randrange(2, 8)))
    return " ".join(rng.choice(WORDS) for _ in range(rng.randrange(1, 5)))


DRESSES = [truecolor, palette, styled, link, lambda _rng, t: t]


def dress(rng, text):
    return rng.choice(DRESSES)(rng, text)


def scroll_line(rng, n, cols):
    level, colour = rng.choice(LEVELS)
    parts = [
        f"{ESC}[2m{n:09d}{ESC}[0m ",
        f"{ESC}[1;{colour}m{level:5}{ESC}[0m ",
    ]
    for _ in range(rng.randrange(3, 12)):
        parts.append(dress(rng, piece(rng)))
        parts.append(" ")
    if rng.randrange(8) == 0:
        # Longer than the screen: it wraps.
        parts.append(truecolor(rng, "=" * (cols + rng.randrange(cols))))
    line = "".join(parts)
    if rng.randrange(12) == 0:
        # A progress bar redrawn in place, then the line written over it.
        bar = "".join(
            f"\r{ESC}[46m{' ' * step}{ESC}[0m{step * 5:3d}%" for step in range(0, 21, 4)
        )
        line = f"{bar}\r{ESC}[K{line}"
    return line + "\r\n"


def redraw_frame(rng, frame, cols, rows):
    out = [f"{ESC}[H"]
    for y in range(rows - 1):
        r = (frame * 3 + y * 7) % 256
        g = (frame * 5 + y * 3) % 256
        cells = []
        width = 0
        while width < cols - 12:
            text = piece(rng)
            cells.append(f"{ESC}[38;2;{r};{g};{(r + g) % 256}m{text}")
            width += len(text) + 1
        row = f"{y:3d} {frame:6d} " + " ".join(cells)
        out.append(f"{row}{ESC}[0m{ESC}[K\r\n")
    out.append(f"{ESC}[7m frame {frame} {ESC}[0m{ESC}[K")
    return "".join(out)


def main():
    args = sys.argv[1:]
    switches = {arg for arg in args if arg.startswith("no-")}
    plain = [arg for arg in args if not arg.startswith("no-")]
    mib = float(plain[0]) if len(plain) > 0 else 64.0
    mode = plain[1] if len(plain) > 1 else "scroll"
    if mode not in ("scroll", "redraw"):
        sys.exit(f"stress.py: the mode is scroll or redraw, not {mode}")
    if "no-links" in switches:
        DRESSES.remove(link)
    if "no-truecolor" in switches:
        DRESSES.remove(truecolor)
    cols, rows = shutil.get_terminal_size((80, 24))
    target = int(mib * (1 << 20))
    rng = random.Random(42)

    print(f"building {mib:g} MiB of {mode} output...", file=sys.stderr)
    chunks = []
    size = 0
    count = 0
    while size < target:
        if mode == "scroll":
            text = scroll_line(rng, count, cols)
        else:
            text = redraw_frame(rng, count, cols, rows)
        data = text.encode()
        chunks.append(data)
        size += len(data)
        count += 1
    data = b"".join(chunks)
    del chunks

    out = sys.stdout.buffer
    fd = out.fileno()
    if mode == "redraw":
        out.write(f"{ESC}[?1049h{ESC}[2J".encode())
        out.flush()
    view = memoryview(data)
    started = time.monotonic()
    for start in range(0, len(view), 1 << 16):
        chunk = view[start:start + (1 << 16)]
        while chunk:
            written = os.write(fd, chunk)
            chunk = chunk[written:]
    elapsed = time.monotonic() - started
    if mode == "redraw":
        out.write(f"{ESC}[?1049l".encode())
        out.flush()

    unit = "frames" if mode == "redraw" else "lines"
    print(
        f"\n{ESC}[0m{len(data) / (1 << 20):.1f} MiB, {count} {unit} in {elapsed:.2f} s: "
        f"{len(data) / elapsed / 1e6:.0f} MB/s, {count / elapsed:,.0f} {unit}/s "
        f"({cols}x{rows})",
        file=sys.stderr,
    )


if __name__ == "__main__":
    main()
