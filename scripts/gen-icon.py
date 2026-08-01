#!/usr/bin/env python3
"""Generate the app icons — a white database cylinder on a black rounded square,
no letter — for every platform:

    crates/zdb-app/resources/zdb.ico    Windows (embedded by build.rs)
    crates/zdb-app/resources/zdb.icns   macOS (copied into zdb.app by
                                        scripts/package-macos.sh)

Run: python3 scripts/gen-icon.py"""
import os
import struct
from io import BytesIO

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

RES = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "crates", "zdb-app", "resources"))

# --- Windows .ico ---------------------------------------------------------
# Downscale master, then emit a multi-size .ico.
master = img.resize((256, 256), Image.LANCZOS)
sizes = [256, 64, 48, 32, 16]
frames = [master.resize((s, s), Image.LANCZOS) for s in sizes]
out = os.path.join(RES, "zdb.ico")
frames[0].save(out, format="ICO", sizes=[(s, s) for s in sizes], append_images=frames[1:])
print("wrote", out)

# --- macOS .icns ----------------------------------------------------------
# Apple's grid insets the artwork: the rounded square occupies ~824 of the 1024
# canvas, the rest is transparent margin. Without it the icon looks oversized
# next to every other Dock icon.
inset = Image.new("RGBA", (N, N), (0, 0, 0, 0))
art = img.resize((824, 824), Image.LANCZOS)
inset.paste(art, ((N - 824) // 2, (N - 824) // 2), art)

# ICNS = b"icns" + total length, then (4-byte type, 4-byte length-incl-header,
# data) entries. Modern macOS reads PNG payloads for these types, so the file
# can be written on any OS (Pillow's own ICNS writer shells out to macOS-only
# `iconutil`, and we build here on Linux).
ICNS_TYPES = [
    (b"icp4", 16),
    (b"icp5", 32),
    (b"ic07", 128),
    (b"ic08", 256),
    (b"ic09", 512),
    (b"ic10", 1024),  # 512@2x
    (b"ic11", 32),  # 16@2x
    (b"ic12", 64),  # 32@2x
    (b"ic13", 256),  # 128@2x
    (b"ic14", 512),  # 256@2x
]
entries = []
for kind, size in ICNS_TYPES:
    buf = BytesIO()
    inset.resize((size, size), Image.LANCZOS).save(buf, format="PNG")
    png = buf.getvalue()
    entries.append(kind + struct.pack(">I", len(png) + 8) + png)
body = b"".join(entries)
out = os.path.join(RES, "zdb.icns")
with open(out, "wb") as f:
    f.write(b"icns" + struct.pack(">I", len(body) + 8) + body)
print("wrote", out)
