#!/usr/bin/env python3
"""Paint a ratatui TestBackend cell dump (JSON from the prototype harness) to a PNG.

usage: render_png.py <dump.json> <out.png>
Colours: VS Code's default dark terminal palette; a real terminal's will differ.
"""
import json, re, sys
from PIL import Image, ImageDraw, ImageFont

FONT = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf"
FONT_BOLD = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono-Bold.ttf"
SIZE = 18
DEFAULT_FG, DEFAULT_BG = (204, 204, 204), (30, 30, 30)
NAMED = {
    "Black": (0, 0, 0), "Red": (205, 49, 49), "Green": (13, 188, 121), "Yellow": (229, 229, 16),
    "Blue": (36, 114, 200), "Magenta": (188, 63, 188), "Cyan": (17, 168, 205), "Gray": (229, 229, 229),
    "DarkGray": (102, 102, 102), "LightRed": (241, 76, 76), "LightGreen": (35, 209, 139),
    "LightYellow": (245, 245, 67), "LightBlue": (59, 142, 234), "LightMagenta": (214, 112, 214),
    "LightCyan": (41, 184, 219), "White": (255, 255, 255),
}
BOLD, DIM, UNDERLINED, REVERSED = 1, 2, 8, 64


def color(name, default):
    if name == "Reset":
        return default
    if name in NAMED:
        return NAMED[name]
    m = re.fullmatch(r"Rgb\((\d+), (\d+), (\d+)\)", name)
    if m:
        return tuple(int(v) for v in m.groups())
    raise SystemExit(f"unmapped color {name!r}")


def main():
    doc = json.load(open(sys.argv[1]))
    width, height = doc["width"], doc["height"]
    regular, bold = ImageFont.truetype(FONT, SIZE), ImageFont.truetype(FONT_BOLD, SIZE)
    ascent, descent = regular.getmetrics()
    cw, ch = round(regular.getlength("M")), ascent + descent
    img = Image.new("RGB", (width * cw, height * ch), DEFAULT_BG)
    draw = ImageDraw.Draw(img)
    for y in range(height):
        for x in range(width):
            cell = doc["cells"][y * width + x]
            fg, bg = color(cell["fg"], DEFAULT_FG), color(cell["bg"], DEFAULT_BG)
            mods = cell["m"]
            if mods & REVERSED:
                fg, bg = bg, fg
            if mods & DIM:
                fg = tuple((2 * f + b) // 3 for f, b in zip(fg, bg))
            x0, y0 = x * cw, y * ch
            if bg != DEFAULT_BG:
                draw.rectangle([x0, y0, x0 + cw - 1, y0 + ch - 1], fill=bg)
            sym = cell["s"]
            if not sym or sym == " ":
                continue
            code = ord(sym[0])
            if 0x2581 <= code <= 0x2588:  # lower one-eighth .. full block
                filled = round(ch * (code - 0x2580) / 8)
                draw.rectangle([x0, y0 + ch - filled, x0 + cw - 1, y0 + ch - 1], fill=fg)
            else:
                draw.text((x0, y0), sym, font=bold if mods & BOLD else regular, fill=fg)
            if mods & UNDERLINED:
                draw.line([x0, y0 + ch - 2, x0 + cw - 1, y0 + ch - 2], fill=fg)
    img.quantize(colors=96, method=Image.Quantize.MEDIANCUT, dither=Image.Dither.NONE).save(sys.argv[2], optimize=True)
    print(sys.argv[2], img.size)


main()
