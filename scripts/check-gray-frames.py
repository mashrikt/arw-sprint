#!/usr/bin/env python3
"""Read-only regression check for FastCull's synthetic gray-fixture captures.

Run: python3 scripts/check-gray-frames.py bench-results/filter-smoke-fixed
Only use the synthetic 8x8 gray fixture: real photographs may legitimately be red.
"""

import argparse
from collections import Counter
from pathlib import Path
import re
import sys


PATCH = 32
MAX_CAPTURE_BYTES = 256 * 1024 * 1024
WHITESPACE = b" \t\r\n\v\f"


def read_p6(path):
    with path.open("rb") as source:
        data = source.read(MAX_CAPTURE_BYTES + 1)
    if len(data) > MAX_CAPTURE_BYTES:
        raise ValueError("capture exceeds 256 MiB")

    offset = 0

    def token():
        nonlocal offset
        while offset < len(data):
            if data[offset] in WHITESPACE:
                offset += 1
            elif data[offset] == ord("#"):
                end = data.find(b"\n", offset)
                if end < 0:
                    raise ValueError("unterminated PPM header comment")
                offset = end + 1
            else:
                break
        start = offset
        while offset < len(data) and data[offset] not in WHITESPACE + b"#":
            offset += 1
        if start == offset:
            raise ValueError("truncated PPM header")
        return data[start:offset]

    if token() != b"P6":
        raise ValueError("expected binary P6 PPM")
    width, height, maximum = (int(token()) for _ in range(3))
    if width < PATCH * 2 or height < PATCH * 2:
        raise ValueError("capture is too small for separate 32x32 corner patches")
    if maximum != 255:
        raise ValueError("expected 8-bit PPM channels (maximum 255)")
    if offset >= len(data) or data[offset] not in WHITESPACE:
        raise ValueError("missing PPM raster separator")
    # Consume one separator, never arbitrary whitespace: raster bytes can be
    # ASCII spaces/newlines themselves. Accept CRLF as one line separator.
    offset += 2 if data[offset:offset + 2] == b"\r\n" else 1
    pixels = data[offset:]
    expected = width * height * 3
    if len(pixels) != expected:
        raise ValueError(f"raster is {len(pixels)} bytes; expected {expected}")
    return width, height, pixels


def inspect(width, height, pixels):
    red_count = 0
    largest_bias = 0
    examples = []
    for index, (red, green, blue) in enumerate(
        zip(pixels[0::3], pixels[1::3], pixels[2::3])
    ):
        if red > green and red > blue:
            red_count += 1
            largest_bias = max(largest_bias, red - max(green, blue))
            if len(examples) < 4:
                examples.append((index % width, index // width, red, green, blue))

    corners = []
    for x_start in (0, width - PATCH):
        colors = Counter()
        for y in range(PATCH):
            for x in range(x_start, x_start + PATCH):
                offset = (y * width + x) * 3
                colors[tuple(pixels[offset:offset + 3])] += 1
        corners.append(colors)

    failures = []
    if red_count:
        failures.append(
            f"red-biased pixels={red_count}/{width * height}, "
            f"maximum R-max(G,B)={largest_bias}, "
            f"first (x,y,R,G,B)={examples}"
        )
    for label, colors in zip(("top-left", "top-right"), corners):
        nonneutral = sum(count for (r, g, b), count in colors.items() if r != g or g != b)
        if len(colors) != 1 or nonneutral:
            failures.append(
                f"{label} 32x32: distinct colors={len(colors)}, "
                f"nonneutral pixels={nonneutral}/{PATCH * PATCH}, "
                f"most common (RGB,count)={colors.most_common(4)}"
            )
    if corners[0] != corners[1]:
        failures.append("top-left and top-right background patches differ")
    return failures, corners


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("captures", type=Path, nargs="+", help="PPM files or capture directories")
    args = parser.parse_args()
    paths = []
    for capture in args.captures:
        if capture.is_dir():
            found = list(capture.glob("frame-*.ppm"))
            if not found:
                parser.error(f"no frame-*.ppm captures in {capture}")
            paths.extend(found)
        else:
            paths.append(capture)
    paths = sorted(set(paths), key=lambda p: (str(p.parent), [int(x) if x.isdigit() else x for x in re.split(r"(\d+)", p.name)]))

    failed = 0
    for path in paths:
        try:
            width, height, pixels = read_p6(path)
            failures, corners = inspect(width, height, pixels)
        except (OSError, ValueError) as error:
            failures = [str(error)]
        if failures:
            failed += 1
            print(f"FAIL {path}")
            for failure in failures:
                print(f"  {failure}")
        else:
            print(
                f"PASS {path}: {width}x{height}, red-biased=0/{width * height}, "
                f"both 32x32 backgrounds={next(iter(corners[0]))}"
            )
    print(f"Checked {len(paths)} gray-fixture captures: {len(paths) - failed} passed, {failed} failed.")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
