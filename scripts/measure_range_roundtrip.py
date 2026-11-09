#!/usr/bin/env python3
"""CPU-only BT.709 nonlinear RGB -> 8-bit YCbCr -> RGB quantization experiment.

No chroma subsampling, filtering, GPU processing, transfer conversion, or codec.
Uses a deterministic 17-level RGB cube plus all 256 neutral gray levels.
"""
import math

levels = list(range(0, 256, 16)) + [255]
pattern = [(r, g, b) for r in levels for g in levels for b in levels]
pattern += [(v, v, v) for v in range(256)]


def quantize(value):
    return max(0, min(255, math.floor(value + 0.5)))


for full in (False, True):
    squared_error = 0.0
    peak_error = 0.0
    for rgb in pattern:
        r, g, b = (v / 255 for v in rgb)
        y = 0.2126 * r + 0.7152 * g + 0.0722 * b
        cb, cr = (b - y) / 1.8556, (r - y) / 1.5748
        offset, ys, cs = (0, 255, 255) if full else (16, 219, 224)
        y = (quantize(offset + ys * y) - offset) / ys
        cb = (quantize(128 + cs * cb) - 128) / cs
        cr = (quantize(128 + cs * cr) - 128) / cs
        r, b = y + 1.5748 * cr, y + 1.8556 * cb
        g = (y - 0.2126 * r - 0.0722 * b) / 0.7152
        for source, decoded in zip(rgb, (r, g, b)):
            error = source - max(0.0, min(255.0, decoded * 255))
            squared_error += error * error
            peak_error = max(peak_error, abs(error))
    mse = squared_error / (3 * len(pattern))
    print(f"{'full' if full else 'limited'}: pixels={len(pattern)}, MSE={mse:.6f}, "
          f"PSNR={10 * math.log10(255**2 / mse):.4f} dB, max_error={peak_error:.4f}/255")
