#!/usr/bin/env python3
"""Regenerate the progressive JPEG fixtures used by `src/jpeghuff.rs` tests.

    python3 fixtures/jpeg/generate.py

Needs Pillow, and `jpegtran` (libjpeg-turbo) for the restart-interval and
custom-scan-script variants. The outputs are small and committed, so the
test suite has no external dependency.
"""
import math
import pathlib
import subprocess

from PIL import Image

HERE = pathlib.Path(__file__).resolve().parent


def gradient(w, h, mode):
    img = Image.new(mode, (w, h))
    px = img.load()
    for y in range(h):
        for x in range(w):
            # Smooth-ish content with a little high-frequency detail, so the
            # AC bands are neither empty nor uniformly noisy.
            a = int(127 + 100 * math.sin(x / 9.0) * math.cos(y / 7.0))
            b = int(127 + 90 * math.sin((x + y) / 5.0))
            c = int((x * 3 + y * 5) % 256)
            px[x, y] = a if mode == "L" else (a, b, c)
    return img


def save(name, img, **kw):
    path = HERE / name
    img.save(path, "JPEG", **kw)
    return path


def main():
    color = gradient(96, 64, "RGB")
    grey = gradient(80, 48, "L")

    save("prog_color.jpg", color, progressive=True, quality=72)
    save("prog_gray.jpg", grey, progressive=True, quality=72)
    # 4:4:4 keeps every component on the same block grid; 4:2:0 (the default
    # above) exercises the subsampled non-interleaved AC grids.
    save("prog_color444.jpg", color, progressive=True, quality=80, subsampling=0)
    save("seq_color.jpg", color, progressive=False, quality=72)

    # Restart intervals inside progressive scans.
    src = save("_tmp.jpg", color, progressive=False, quality=72)
    subprocess.run(
        ["jpegtran", "-progressive", "-restart", "1B", "-outfile",
         str(HERE / "prog_restart.jpg"), str(src)],
        check=True,
    )
    # A scan script with a deep successive-approximation ladder, so DC and AC
    # refinement passes both appear several times.
    script = HERE / "_scans.txt"
    script.write_text(
        """
0: 0 0 0 2;
1: 0 0 0 2;
2: 0 0 0 2;
0: 1 8 0 2;
1: 1 8 0 2;
2: 1 8 0 2;
0: 9 63 0 2;
1: 9 63 0 2;
2: 9 63 0 2;
0: 0 0 2 1;
1: 0 0 2 1;
2: 0 0 2 1;
0: 1 63 2 1;
1: 1 63 2 1;
2: 1 63 2 1;
0: 0 0 1 0;
1: 0 0 1 0;
2: 0 0 1 0;
0: 1 63 1 0;
1: 1 63 1 0;
2: 1 63 1 0;
"""
    )
    subprocess.run(
        ["jpegtran", "-progressive", "-scans", str(script), "-outfile",
         str(HERE / "prog_deep.jpg"), str(src)],
        check=True,
    )
    src.unlink()
    script.unlink()
    for p in sorted(HERE.glob("*.jpg")):
        print(f"{p.stat().st_size:>7}  {p.name}")


if __name__ == "__main__":
    main()
