#!/usr/bin/env python3
"""Regenerate the CMYK/YCCK JPEG fixtures used by the src/lib.rs CMYK tests.

    python3 fixtures/jpeg/generate_cmyk.py

Needs Pillow. Produces the two *source* fixtures (both Adobe APP14
transform = 0, i.e. samples stored as plain CMYK); the YCCK (transform = 2)
fixture is derived from cmyk_plain.jpg by `examples/gen_cmyk_ycck.rs`, which
is the only YCCK encoder available here — see docs/CMYK-JPEG.md for the
independent djpeg/PIL cross-check that validates it.

The four channels are deliberately given very different spatial signatures
(low-frequency sine, diagonal ramp, vertical ramp, radial blob) so that ANY
channel swap or inversion in the pipeline shows up as a large per-channel
difference rather than hiding inside plausible-looking colour.
"""
import math
import pathlib

from PIL import Image

HERE = pathlib.Path(__file__).resolve().parent


def cmyk(w, h):
    img = Image.new("CMYK", (w, h))
    px = img.load()
    cx, cy = w / 2.0, h / 2.0
    r_max = math.hypot(cx, cy)
    for y in range(h):
        for x in range(w):
            c = int(127 + 100 * math.sin(x / 9.0) * math.cos(y / 7.0))
            m = int(255 * ((x + y) % 128) / 128.0)
            ye = int(255 * y / max(h - 1, 1))
            k = int(220 * (1.0 - math.hypot(x - cx, y - cy) / r_max))
            px[x, y] = (c, m, ye, max(k, 0))
    return img


def main():
    # (a) small, straight CMYK, no subsampling: the canonical transform = 0.
    cmyk(96, 64).save(HERE / "cmyk_plain.jpg", "JPEG", quality=92)
    # (c) over-resolution source for the end-to-end downsample test.
    cmyk(640, 480).save(HERE / "cmyk_large.jpg", "JPEG", quality=92)
    print("wrote cmyk_plain.jpg, cmyk_large.jpg")


if __name__ == "__main__":
    main()
