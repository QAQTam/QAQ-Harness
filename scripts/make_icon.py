# Generate assets/qaqh-harness.ico from assets/icon-source.png.
#
# The source artwork is a rounded-square logo on a solid black background;
# the border-connected black region is flood-filled to transparency so the
# rounded corners render cleanly at every size. Output ICO contains
# 16/24/32/48/64/128/256 px frames (LANCZOS downscales).
#
# Usage: python scripts/make_icon.py

from pathlib import Path

from PIL import Image, ImageDraw

ROOT = Path(__file__).resolve().parent.parent
SRC = ROOT / "assets" / "icon-source.png"
OUT = ROOT / "assets" / "qaqh-harness.ico"
SIZES = [16, 24, 32, 48, 64, 128, 256]
BLACK_THRESHOLD = 40  # 0-255; pixels darker than this near the border are background


def square_crop(img: Image.Image) -> Image.Image:
    side = min(img.size)
    left = (img.width - side) // 2
    top = (img.height - side) // 2
    return img.crop((left, top, left + side, top + side))


def knock_out_border_black(img: Image.Image) -> Image.Image:
    rgba = img.convert("RGBA")
    w, h = rgba.size
    seeds = [(0, 0), (w - 1, 0), (0, h - 1), (w - 1, h - 1)]
    for seed in seeds:
        ImageDraw.floodfill(rgba, seed, (0, 0, 0, 0), thresh=BLACK_THRESHOLD)
    return rgba


def main() -> None:
    base = knock_out_border_black(square_crop(Image.open(SRC)))
    frames = [base.resize((s, s), Image.LANCZOS) for s in SIZES]
    frames[-1].save(OUT, format="ICO", append_images=frames[:-1])
    print(f"wrote {OUT} with {len(SIZES)} sizes: {SIZES}")


if __name__ == "__main__":
    main()
