#!/usr/bin/env python3
"""Regenerate the PWA's PNG icons (web/static/icon-*.png). Needs Pillow.

The drawing mirrors web/static/icon.svg: a rounded green tile with a white house.
Run it after changing the icon; the PNGs are committed.
"""
from pathlib import Path

from PIL import Image, ImageDraw

GREEN = (29, 107, 82, 255)
WHITE = (255, 255, 255, 255)
OUT = Path(__file__).resolve().parent.parent / "web" / "static"


def draw(size: int, maskable: bool) -> Image.Image:
    scale = 4  # supersample, then shrink, for smooth edges
    s = size * scale
    image = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    d = ImageDraw.Draw(image)
    # Maskable icons fill the square; the launcher applies its own mask.
    d.rounded_rectangle([0, 0, s - 1, s - 1], radius=0 if maskable else int(s * 112 / 512), fill=GREEN)
    # Keep the artwork inside the maskable safe zone (the central 80%).
    k = (0.72 if maskable else 1.0) * s / 512
    off = (s - 512 * k) / 2

    def p(x, y):
        return (off + x * k, off + y * k)

    house = [p(136, 372), p(136, 196), p(256, 112), p(376, 196), p(376, 372), p(292, 372), p(292, 268), p(220, 268), p(220, 372)]
    d.polygon(house, fill=WHITE)
    cx, cy, r = *p(256, 204), 26 * k
    d.ellipse([cx - r, cy - r, cx + r, cy + r], fill=GREEN)
    return image.resize((size, size), Image.LANCZOS)


for name, size, maskable in [("icon-192.png", 192, False), ("icon-512.png", 512, False), ("icon-maskable-512.png", 512, True)]:
    draw(size, maskable).save(OUT / name, optimize=True)
    print("wrote", OUT / name)
