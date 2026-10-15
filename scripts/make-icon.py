#!/usr/bin/env python3
"""Generates the rzen app icon: a calligraphic ensō on black.

The brush head is heavy at the lower left, the stroke thins clockwise around the
circle and breaks into dry-brush bristles at the tail.

Writes:
  assets/AppIcon.icon/Assets/enso.svg  glyph layer for the macOS 26 Icon Composer icon
  assets/icon.svg                      flat icon on the Big Sur grid (README, older macOS)
  assets/AppIcon.icns                  fallback for macOS 13-15 (needs rsvg-convert)
"""
import math, os, shutil, subprocess, tempfile

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..")
CX, CY = 512.0, 516.0
R = 292.0            # centreline radius
W = 120.0            # max stroke width
A0 = 118.0           # start angle (deg, screen coords, clockwise positive)
SWEEP = 322.0
TILT = math.radians(-9)
SQUASH = 0.965
N = 360

# Bristles: (lateral centre in [-1,1], half-width, dry start, end, gap depth)
RIBBONS = [
    (-0.86, 0.16, 0.50, 0.900, 0.70),
    (-0.58, 0.17, 0.62, 0.975, 0.62),
    (-0.29, 0.17, 0.56, 0.940, 0.72),
    ( 0.00, 0.18, 0.66, 1.000, 0.50),
    ( 0.29, 0.17, 0.54, 0.955, 0.70),
    ( 0.58, 0.17, 0.60, 0.915, 0.66),
    ( 0.86, 0.16, 0.48, 0.860, 0.74),
]

def smooth(x):
    x = max(0.0, min(1.0, x))
    return x * x * (3 - 2 * x)

def center(s):
    a = math.radians(A0 + SWEEP * s)
    r = R * (1 + 0.018 * math.sin(2 * math.pi * 1.4 * s + 0.6)) - 22 * s ** 3
    x, y = r * math.cos(a), r * math.sin(a) * SQUASH
    return (CX + x * math.cos(TILT) - y * math.sin(TILT),
            CY + x * math.sin(TILT) + y * math.cos(TILT))

def width(s):
    head = 0.88 + 0.12 * smooth(s / 0.07)
    body = 1 - 0.5 * smooth((s - 0.28) / 0.6)
    tail = 1 - smooth((s - 0.9) / 0.1) ** 0.7
    return W * head * body * tail * (1 + 0.035 * math.sin(7.5 * math.pi * s))

def frame(s, ds=1e-4):
    x0, y0 = center(max(0, s - ds)); x1, y1 = center(min(1, s + ds))
    tx, ty = x1 - x0, y1 - y0; L = math.hypot(tx, ty)
    return tx / L, ty / L, -ty / L, tx / L   # tangent, normal

def ribbon(c, hw, sd, se, g):
    left, right = [], []
    for i in range(N + 1):
        s = se * i / N
        x, y = center(s); _, _, nx, ny = frame(s)
        w = width(s) / 2
        k = 1 - g * smooth((s - sd) / (se - sd))            # bristles thin and part
        end = 1 - smooth((s - (se - 0.06)) / 0.06)          # each one ends in a point
        h = hw * 1.75 * k * (end if se < 1 else 1)
        lo, hi = max(-1, c - h), min(1, c + h)
        left.append((x + nx * lo * w, y + ny * lo * w))
        right.append((x + nx * hi * w, y + ny * hi * w))
    return left + right[::-1]

def head():
    x, y = center(0); tx, ty, nx, ny = frame(0)
    rw, rt = width(0) / 2, width(0) * 0.47
    return [(x + tx * math.cos(t) * rt + nx * math.sin(t) * rw,
             y + ty * math.cos(t) * rt + ny * math.sin(t) * rw)
            for t in (2 * math.pi * i / 64 for i in range(64))]

def path(pts):
    return "M" + " L".join(f"{x:.1f},{y:.1f}" for x, y in pts) + "Z"

def glyph(indent):
    return "\n".join(f'{indent}<path d="{path(p)}"/>' for p in [head()] + [ribbon(*r) for r in RIBBONS])

def squircle(x, y, size, n=5.0, steps=720):
    a = size / 2; cx, cy = x + a, y + a; pts = []
    for i in range(steps):
        t = 2 * math.pi * i / steps
        c, s = math.cos(t), math.sin(t)
        pts.append((cx + a * math.copysign(abs(c) ** (2 / n), c),
                    cy + a * math.copysign(abs(s) ** (2 / n), s)))
    return path(pts)

def layer_svg():
    return ('<svg xmlns="http://www.w3.org/2000/svg" width="1024" height="1024" viewBox="0 0 1024 1024">\n'
            f'  <g fill="#ffffff">\n{glyph("    ")}\n  </g>\n</svg>\n')

def flat_svg():
    # Big Sur grid: 824pt body inset 100pt.
    body = squircle(100, 100, 824)
    return f'''<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1024 1024">
  <defs>
    <linearGradient id="bg" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#242424"/>
      <stop offset="1" stop-color="#070707"/>
    </linearGradient>
    <linearGradient id="ink" gradientUnits="userSpaceOnUse" x1="0" y1="180" x2="0" y2="860">
      <stop offset="0" stop-color="#ffffff"/>
      <stop offset="1" stop-color="#e6e6e6"/>
    </linearGradient>
    <filter id="drop" x="-10%" y="-10%" width="120%" height="125%" color-interpolation-filters="sRGB">
      <feGaussianBlur in="SourceAlpha" stdDeviation="14"/>
      <feOffset dy="10"/>
      <feComponentTransfer><feFuncA type="linear" slope="0.3"/></feComponentTransfer>
      <feMerge><feMergeNode/><feMergeNode in="SourceGraphic"/></feMerge>
    </filter>
  </defs>
  <path filter="url(#drop)" fill="url(#bg)" d="{body}"/>
  <path fill="none" stroke="#ffffff" stroke-opacity="0.09" stroke-width="2" d="{squircle(101, 101, 822)}"/>
  <g fill="url(#ink)" transform="translate(100 100) scale({824 / 1024:.6f})">
{glyph("    ")}
  </g>
</svg>
'''

def icns(svg):
    tmp = tempfile.mkdtemp()
    iconset = os.path.join(tmp, "AppIcon.iconset")
    os.mkdir(iconset)
    for pt in (16, 32, 128, 256, 512):
        for scale in (1, 2):
            name = f"icon_{pt}x{pt}{'@2x' if scale == 2 else ''}.png"
            subprocess.run(["rsvg-convert", "-w", str(pt * scale), svg, "-o",
                            os.path.join(iconset, name)], check=True)
    subprocess.run(["iconutil", "-c", "icns", iconset, "-o",
                    os.path.join(ROOT, "assets/AppIcon.icns")], check=True)
    shutil.rmtree(tmp)

if __name__ == "__main__":
    def write(rel, text):
        p = os.path.join(ROOT, rel)
        os.makedirs(os.path.dirname(p), exist_ok=True)
        open(p, "w").write(text)
    write("assets/AppIcon.icon/Assets/enso.svg", layer_svg())
    write("assets/icon.svg", flat_svg())
    icns(os.path.join(ROOT, "assets/icon.svg"))
