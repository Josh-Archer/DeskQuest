"""Pack Blender PNGs as straight-alpha, top-down RGBA8 and create a review board.

Run with system Python and Pillow after build.py. Rust premultiplies the RGBA
once when it builds each cached overlay, as required by its compositor flags.
"""

import hashlib
import json
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont


ROOT = Path(__file__).resolve().parent
ASSETS = ROOT.parents[1] / "crates/client/assets/spatial-controls"
ASSETS.mkdir(parents=True, exist_ok=True)
manifest = json.loads((ROOT / "manifest.json").read_text())
for name, spec in manifest["assets"].items():
    image = Image.open(ROOT / "renders" / f"{name}.png").convert("RGBA")
    assert image.size == tuple(spec["pixels"])
    raw = image.tobytes()
    (ASSETS / f"{name}.rgba").write_bytes(raw)
    spec["rgba_bytes"] = len(raw)
    spec["sha256"] = hashlib.sha256(raw).hexdigest()
(ROOT / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")


def font(size, bold=False):
    suffix = "-Bold" if bold else ""
    return ImageFont.truetype(f"/usr/share/fonts/truetype/dejavu/DejaVuSans{suffix}.ttf", size)


board = Image.new("RGBA", (1800, 1280), "#10191f")
draw = ImageDraw.Draw(board)


def label(xy, text, size=23, fill="#9bafb9", bold=False):
    draw.text(xy, text, font=font(size, bold), fill=fill)


def paste_fit(path, box):
    image = Image.open(path).convert("RGBA")
    image.thumbnail((box[2] - box[0], box[3] - box[1]), Image.Resampling.LANCZOS)
    board.alpha_composite(image, (box[0] + (box[2] - box[0] - image.width) // 2,
                                 box[1] + (box[3] - box[1] - image.height) // 2))


label((66, 44), "DESKQUEST", 23, "#65d6c0", True)
paste_fit(ROOT.parents[1] / "assets/branding/linux-quest-logo.png", (1590, 28, 1750, 188))
label((66, 87), "A little more tactile.", 62, "#edf4f6", True)
label((68, 173), "Satin edges. Recessed grips. A precise point of focus.", 26)
for box in ((60, 245, 830, 610), (850, 245, 1285, 610), (1305, 245, 1740, 610)):
    draw.rounded_rectangle(box, 22, fill="#1c2a32", outline="#34424b", width=2)
label((86, 269), "01  /  MOVE", 20, "#65d6c0", True)
label((876, 269), "02  /  RESIZE", 20, "#65d6c0", True)
label((1331, 269), "03  /  POINT", 20, "#65d6c0", True)
paste_fit(ROOT / "renders/grip-model.png", (80, 330, 810, 500))
paste_fit(ROOT / "renders/corner-model.png", (930, 310, 1205, 520))
paste_fit(ROOT / "renders/cursor-model.png", (1390, 320, 1660, 520))
label((86, 523), "Beveled grip + raised ribs", 26, "#edf4f6")
label((86, 565), "Authoring model: 24 cm / 4.4 mm shell", 18)
label((876, 523), "Curved corner rail", 25, "#edf4f6")
label((876, 565), "Authoring model: 7 cm / teal guide", 18)
label((1331, 523), "Open precision ring", 25, "#edf4f6")
label((1331, 565), "2.5 cm footprint  /  center dot", 18)

label((66, 657), "COMPOSITOR TEXTURES / INTERACTION STUDY", 20, "#65d6c0", True)
draw.rounded_rectangle((60, 708, 1095, 1157), 22, fill="#17232b")
draw.rounded_rectangle((160, 768, 1000, 1088), 25, fill="#233640", outline="#53717e", width=2)
label((191, 792), "Workspace", 24, "#d7e5e9", True)
for i, width in enumerate((450, 620, 530, 340)):
    draw.rounded_rectangle((192, 852 + i * 42, 192 + width, 862 + i * 42), 5, fill="#3f5761")
grip = Image.open(ROOT / "renders/grip.png").convert("RGBA")
corner = Image.open(ROOT / "renders/corner.png").convert("RGBA")
cursor = Image.open(ROOT / "renders/cursor.png").convert("RGBA")
board.alpha_composite(grip.resize((118, 10), Image.Resampling.LANCZOS), (521, 1113))
board.alpha_composite(corner.resize((97, 97), Image.Resampling.LANCZOS), (137, 745))
board.alpha_composite(cursor.resize((54, 54), Image.Resampling.LANCZOS), (670, 970))

label((1140, 735), "At rest / in use", 29, "#edf4f6", True)
label((1140, 783), "Same geometry, stronger presence on grab.", 18)
for y, background in ((840, "#0a1117"), (962, "#dce5e8")):
    draw.rounded_rectangle((1135, y, 1740, y + 104), 12, fill=background)
    for x, opacity in ((1155, 0.60), (1455, 0.95)):
        handle = grip.resize((260, 22), Image.Resampling.LANCZOS)
        handle.putalpha(handle.getchannel("A").point(lambda a: round(a * opacity)))
        board.alpha_composite(handle, (x, y + 41))
label((1140, 1099), "Baked lighting; existing hover and grab behavior.", 18)
label((66, 1205), "BLENDER AUTHORING PREVIEW  /  Runtime uses baked quads, not live 3D meshes. Headset appearance unverified.", 20)
board.convert("RGB").save(ROOT / "preview.png")
