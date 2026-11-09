#!/usr/bin/env python3
"""Generate a 1920x1080 lossless test frame. Requires Pillow (pip install pillow).

Usage: python scripts/gen_sharp_test_frame.py [output.png]
Use the same generated PNG for all comparisons; font rasterizers vary by host.
"""
import sys
from PIL import Image, ImageDraw, ImageFont


def main():
    image = Image.new("RGB", (1920, 1080), "white")
    draw = ImageDraw.Draw(image)
    # Bundled Pillow bitmap font avoids a dependency on installed system fonts.
    font = ImageFont.load_default(size=14)
    colors = ["black", "red", "blue", "green", "magenta", "cyan"]
    for row, color in enumerate(colors):
        y = 24 + row * 130
        draw.text((24, y), "Desktop sharpness: Il1 O0 RGB 0123456789 / public fn main() {}", font=font, fill=color)
        draw.rectangle((24, y + 24, 950, y + 100), fill="black")
        draw.text((32, y + 32), "White / colored text on black: Il1 O0 0123456789", font=font, fill="white" if row == 0 else color)
        for x in range(1000, 1400, 2):
            draw.line((x, y, x, y + 99), fill=color, width=1)
        for offset in range(0, 100, 2):
            draw.line((1440, y + offset, 1880, y + offset), fill=color, width=1)
    for x in range(24, 1896):
        level = round((x - 24) * 255 / 1871)
        draw.line((x, 830, x, 910), fill=(level, level, level))
    for i, level in enumerate([0, 1, 2, 8, 16, 17, 32, 128, 219, 234, 235, 246, 253, 254, 255]):
        x = 24 + i * 124
        draw.rectangle((x, 940, x + 115, 1010), fill=(level,) * 3)
        draw.text((x, 1020), str(level), font=font, fill="black")
    output = sys.argv[1] if len(sys.argv) > 1 else "sharp-test.png"
    image.save(output)
    print(f"Wrote {output}: 1920x1080 RGB, no scaling")


if __name__ == "__main__":
    main()
