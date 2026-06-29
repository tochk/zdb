#!/usr/bin/env python3
"""Generate crates/zdb-app/resources/zdb.ico: a white database cylinder on a
black rounded square. No letter. Run: python3 scripts/gen-icon.py"""
import os
from PIL import Image, ImageDraw

N = 1024
img = Image.new("RGBA", (N, N), (0, 0, 0, 0))
d = ImageDraw.Draw(img)

# Black rounded-square background.
pad = 36
d.rounded_rectangle([pad, pad, N - pad, N - pad], radius=190, fill=(0, 0, 0, 255))

# Database cylinder geometry.
cx = N // 2
rx = 250
ry = 80
top_y = 322
bot_y = 702
sw = 30
stroke = (255, 255, 255, 255)
face_top = (233, 239, 250, 255)
face_body = (196, 209, 233, 255)

# Body (sides + flat fill between the two ellipses) and rounded bottom.
d.rectangle([cx - rx, top_y, cx + rx, bot_y], fill=face_body)
d.ellipse([cx - rx, bot_y - ry, cx + rx, bot_y + ry], fill=face_body)
# Bottom front edge.
d.arc([cx - rx, bot_y - ry, cx + rx, bot_y + ry], 0, 180, fill=stroke, width=sw)
# Side lines.
d.line([cx - rx, top_y, cx - rx, bot_y], fill=stroke, width=sw)
d.line([cx + rx, top_y, cx + rx, bot_y], fill=stroke, width=sw)

# Two band arcs (the cylinder's stacked discs).
for by in (top_y + 128, top_y + 256):
    d.arc([cx - rx, by - ry, cx + rx, by + ry], 0, 180, fill=stroke, width=sw)

# Top disc (drawn last so it sits above the body).
d.ellipse([cx - rx, top_y - ry, cx + rx, top_y + ry], fill=face_top, outline=stroke, width=sw)

# Downscale master, then emit a multi-size .ico.
master = img.resize((256, 256), Image.LANCZOS)
sizes = [256, 64, 48, 32, 16]
frames = [master.resize((s, s), Image.LANCZOS) for s in sizes]
out = os.path.join(os.path.dirname(__file__), "..", "crates", "zdb-app", "resources", "zdb.ico")
out = os.path.abspath(out)
frames[0].save(out, format="ICO", sizes=[(s, s) for s in sizes], append_images=frames[1:])
print("wrote", out)
