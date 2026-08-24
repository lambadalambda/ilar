#!/usr/bin/env python3
"""Render `tmux capture-pane -e -p` output as an SVG.

Used for documentation screenshots:

    tmux capture-pane -e -p -t <pane> | scripts/ansi2svg.py > docs/assets/shot.svg

Understands the SGR subset a modern TUI emits: reset, bold, dim,
truecolor and 256-color foreground/background. Each run is positioned
on a character grid with textLength, so box drawing stays aligned in
any renderer.
"""

import html
import re
import sys

CELL_W = 8.4
CELL_H = 19.0
FONT_SIZE = 14
PAD = 12
DEFAULT_FG = "#c7ccd1"
DEFAULT_BG = "#101215"
FONT = "SF Mono, JetBrains Mono, Menlo, Consolas, monospace"

SGR = re.compile(r"\x1b\[([0-9;]*)m")
OTHER_ESCAPES = re.compile(r"\x1b(\][^\x07]*\x07|\[[0-9;?]*[A-Za-ln-z])")

CUBE = [0, 95, 135, 175, 215, 255]


def color_256(index: int) -> str:
    basic = [
        "#000000", "#cc0000", "#4e9a06", "#c4a000", "#3465a4", "#75507b",
        "#06989a", "#d3d7cf", "#555753", "#ef2929", "#8ae234", "#fce94f",
        "#729fcf", "#ad7fa8", "#34e2e2", "#eeeeec",
    ]
    if index < 16:
        return basic[index]
    if index < 232:
        index -= 16
        r, g, b = CUBE[index // 36], CUBE[index % 36 // 6], CUBE[index % 6]
        return f"#{r:02x}{g:02x}{b:02x}"
    grey = 8 + (index - 232) * 10
    return f"#{grey:02x}{grey:02x}{grey:02x}"


class Style:
    def __init__(self):
        self.fg = None
        self.bg = None
        self.bold = False
        self.dim = False

    def key(self):
        return (self.fg, self.bg, self.bold, self.dim)

    def apply(self, params):
        codes = [int(part) if part else 0 for part in params.split(";")] or [0]
        i = 0
        while i < len(codes):
            code = codes[i]
            if code == 0:
                self.__init__()
            elif code == 1:
                self.bold = True
            elif code == 2:
                self.dim = True
            elif code == 22:
                self.bold = self.dim = False
            elif code == 39:
                self.fg = None
            elif code == 49:
                self.bg = None
            elif 30 <= code <= 37:
                self.fg = color_256(code - 30)
            elif 90 <= code <= 97:
                self.fg = color_256(code - 90 + 8)
            elif 40 <= code <= 47:
                self.bg = color_256(code - 40)
            elif 100 <= code <= 107:
                self.bg = color_256(code - 100 + 8)
            elif code in (38, 48):
                target = "fg" if code == 38 else "bg"
                if i + 1 < len(codes) and codes[i + 1] == 2 and i + 4 < len(codes):
                    r, g, b = codes[i + 2 : i + 5]
                    setattr(self, target, f"#{r:02x}{g:02x}{b:02x}")
                    i += 4
                elif i + 1 < len(codes) and codes[i + 1] == 5 and i + 2 < len(codes):
                    setattr(self, target, color_256(codes[i + 2]))
                    i += 2
            i += 1


def parse_line(line):
    """Yield (column, text, style-key) runs for one line."""
    style = Style()
    runs = []
    column = 0
    position = 0
    for match in SGR.finditer(line):
        chunk = OTHER_ESCAPES.sub("", line[position : match.start()])
        if chunk:
            runs.append((column, chunk, style.key()))
            column += len(chunk)
        style.apply(match.group(1))
        position = match.end()
    chunk = OTHER_ESCAPES.sub("", line[position:])
    if chunk:
        runs.append((column, chunk, style.key()))
    return runs


def main():
    lines = sys.stdin.read().split("\n")
    while lines and not OTHER_ESCAPES.sub("", SGR.sub("", lines[-1])).strip():
        lines.pop()
    columns = max(
        (sum(len(text) for _, text, _ in parse_line(line)) for line in lines),
        default=80,
    )
    width = columns * CELL_W + PAD * 2
    height = len(lines) * CELL_H + PAD * 2

    out = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width:.0f}" '
        f'height="{height:.0f}" viewBox="0 0 {width:.0f} {height:.0f}">',
        f'<rect width="100%" height="100%" rx="8" fill="{DEFAULT_BG}"/>',
        f'<g font-family="{FONT}" font-size="{FONT_SIZE}" '
        'font-variant-ligatures="none">',
    ]
    for row, line in enumerate(lines):
        y = PAD + row * CELL_H
        baseline = y + CELL_H - 5
        for column, text, (fg, bg, bold, dim) in parse_line(line):
            x = PAD + column * CELL_W
            run_width = len(text) * CELL_W
            if bg and bg != DEFAULT_BG:
                out.append(
                    f'<rect x="{x:.1f}" y="{y:.1f}" width="{run_width:.1f}" '
                    f'height="{CELL_H:.1f}" fill="{bg}"/>'
                )
            if not text.strip():
                continue
            fill = fg or DEFAULT_FG
            attributes = [f'fill="{fill}"']
            if bold:
                attributes.append('font-weight="bold"')
            if dim:
                attributes.append('opacity="0.6"')
            out.append(
                f'<text x="{x:.1f}" y="{baseline:.1f}" {" ".join(attributes)} '
                f'textLength="{run_width:.1f}" lengthAdjust="spacingAndGlyphs" '
                f'xml:space="preserve">{html.escape(text)}</text>'
            )
    out.append("</g></svg>")
    print("\n".join(out))


if __name__ == "__main__":
    main()
