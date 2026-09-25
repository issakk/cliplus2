"""Generates assets/clipplus.ico.

No image library: an .ico is a small, fully specified container, and writing the
bytes directly means the design is reproducible from this file rather than from
a binary someone hand-edited once.

The artwork is deliberately pure geometry — a rounded amber gradient tile with
a white clipboard, the clip as a darker nub on its top edge, and the ClipPlus
plus centred on the board — so it can be reasoned about in normalised
coordinates and verified by reading, at any size, without looking at it.

Sizes: 16-64 as classic BMP entries, 128/256 as PNG entries (Vista and later
read those fine, and they are a fraction of the size). Every shell that scales
— tray, title bar, taskbar, Explorer — can then pick an entry at or near its
real size instead of resampling one bitmap.

Run:  python tools/make_icon.py
"""

import io
import os
import struct
import zlib

GRAD_TOP = (255, 178, 64)     # warm amber, top of the tile
GRAD_BOTTOM = (255, 110, 44)  # deep orange, bottom
BOARD = (255, 255, 255)       # white clipboard
TAB = (233, 118, 35)          # the clip, darker so it reads against the board
PLUS = (66, 128, 244)         # the product's "+", blue: complementary to the
                              # tile, so it pops on the white board instead of
                              # melting into the warm palette

SIZES_BMP = (16, 24, 32, 48, 64)
SIZES_PNG = (128, 256)
SUPERSAMPLE = 4


def rounded_rect(px, py, x0, y0, x1, y1, radius):
    """Point-in-rounded-rectangle test in normalised coordinates."""
    if px < x0 or px > x1 or py < y0 or py > y1:
        return False

    # Only the four corner squares need the distance test.
    cx = x0 + radius if px < x0 + radius else (x1 - radius if px > x1 - radius else px)
    cy = y0 + radius if py < y0 + radius else (y1 - radius if py > y1 - radius else py)

    dx = px - cx
    dy = py - cy
    return dx * dx + dy * dy <= radius * radius


def bg_color(py):
    t = max(0.0, min(1.0, py))
    return tuple(
        round(top + (bottom - top) * t)
        for top, bottom in zip(GRAD_TOP, GRAD_BOTTOM)
    )


def sample(px, py):
    """Returns (r, g, b, a) for a point in normalised [0,1) space."""
    # The tile, then the board, then the parts drawn on the board: the clip
    # sits on its top edge and the plus is centred in the space left below it.
    if not rounded_rect(px, py, 0.0, 0.0, 1.0, 1.0, 0.22):
        return (0, 0, 0, 0)

    tile = bg_color(py) + (255,)

    if rounded_rect(px, py, 0.26, 0.24, 0.74, 0.84, 0.075):
        if rounded_rect(px, py, 0.40, 0.16, 0.60, 0.34, 0.05):
            return TAB + (255,)
        if rounded_rect(px, py, 0.365, 0.535, 0.635, 0.625, 0.045):
            return PLUS + (255,)
        if rounded_rect(px, py, 0.455, 0.445, 0.545, 0.715, 0.045):
            return PLUS + (255,)
        return BOARD + (255,)

    return tile


def render(size):
    """Supersampled RGBA pixels, top-down."""
    step = 1.0 / (size * SUPERSAMPLE)
    pixels = []

    for y in range(size):
        for x in range(size):
            r = g = b = a = 0
            for sy in range(SUPERSAMPLE):
                for sx in range(SUPERSAMPLE):
                    px = (x * SUPERSAMPLE + sx + 0.5) * step
                    py = (y * SUPERSAMPLE + sy + 0.5) * step
                    sr, sg, sb, sa = sample(px, py)
                    r += sr * sa
                    g += sg * sa
                    b += sb * sa
                    a += sa

            if a == 0:
                pixels.append((0, 0, 0, 0))
            else:
                n = SUPERSAMPLE * SUPERSAMPLE
                pixels.append(
                    (round(r / a), round(g / a), round(b / a), round(a / n))
                )

    return pixels


def bmp_image(size, pixels):
    """BITMAPINFOHEADER + 32bpp BGRA (bottom-up) + 1bpp AND mask."""
    header = struct.pack(
        "<IiiHHIIiiII",
        40,          # biSize
        size,        # biWidth
        size * 2,    # biHeight: XOR and AND stacked
        1,           # biPlanes
        32,          # biBitCount
        0,           # BI_RGB
        size * size * 4,
        0,
        0,
        0,
        0,
    )

    xor = bytearray()
    for y in range(size - 1, -1, -1):
        for x in range(size):
            r, g, b, a = pixels[y * size + x]
            # A fully transparent pixel must be all-zero, otherwise some older
            # compositing paths tint the edge.
            if a == 0:
                xor += bytes((0, 0, 0, 0))
            else:
                xor += bytes((b, g, r, a))

    # 1 bit per pixel, each row padded to 4 bytes. Alpha does the real work;
    # this is kept correct for consumers that still read the mask.
    stride = ((size + 31) // 32) * 4
    mask = bytearray()
    for y in range(size - 1, -1, -1):
        row = bytearray(stride)
        for x in range(size):
            if pixels[y * size + x][3] == 0:
                row[x // 8] |= 0x80 >> (x % 8)
        mask += row

    return header + bytes(xor) + bytes(mask)


def png_image(size, pixels):
    """Minimal RGBA PNG: the large icon sizes compress far better than the
    bitmap form, and every shell that matters has read PNG entries since
    Vista. Row filter 0 per scanline."""

    def chunk(tag, payload):
        body = tag + payload
        return (
            struct.pack(">I", len(payload))
            + body
            + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)
        )

    raw = b"".join(
        b"\x00" + bytes(channel for pixel in row for channel in pixel)
        for row in _rows_of(size, pixels)
    )
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )


def _rows_of(size, pixels):
    for y in range(size):
        yield pixels[y * size : (y + 1) * size]


def build():
    images = [(size, bmp_image(size, render(size))) for size in SIZES_BMP]
    images += [(size, png_image(size, render(size))) for size in SIZES_PNG]

    out = io.BytesIO()
    out.write(struct.pack("<HHH", 0, 1, len(images)))

    offset = 6 + 16 * len(images)
    for size, image in images:
        out.write(
            struct.pack(
                "<BBBBHHII",
                size if size < 256 else 0,
                size if size < 256 else 0,
                0,       # palette entries
                0,       # reserved
                1,       # colour planes
                32,      # bits per pixel
                len(image),
                offset,
            )
        )
        offset += len(image)

    for _, image in images:
        out.write(image)

    return out.getvalue()


def main():
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    target = os.path.join(root, "assets", "clipplus.ico")
    os.makedirs(os.path.dirname(target), exist_ok=True)

    data = build()
    with open(target, "wb") as handle:
        handle.write(data)

    print(f"wrote {target} ({len(data)} bytes)")
    for size in SIZES_BMP + SIZES_PNG:
        print(f"  {size}x{size}")

    preview = os.path.join(root, "tools", "icon-preview.png")
    write_preview(preview)
    print(f"wrote {preview}")


def write_png(path, width, height, rows):
    """Minimal RGB PNG writer, so the preview needs no third-party library."""
    import zlib

    raw = b"".join(b"\x00" + bytes(row) for row in rows)

    def chunk(tag, payload):
        body = tag + payload
        return (
            struct.pack(">I", len(payload))
            + body
            + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)
        )

    data = b"\x89PNG\r\n\x1a\n"
    data += chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
    data += chunk(b"IDAT", zlib.compress(raw, 9))
    data += chunk(b"IEND", b"")

    with open(path, "wb") as handle:
        handle.write(data)


def write_preview(path):
    """Tiles each size at an integer zoom over mid grey, so the silhouette and
    the anti-aliased edges can be judged without the real tray."""
    zooms = {16: 8, 24: 6, 32: 5, 48: 4, 64: 3}
    pad = 4

    tiles = [(size, zooms[size], render(size)) for size in zooms]

    width = sum(size * zoom + pad for size, zoom, _ in tiles) + pad
    height = max(size * zoom for size, zoom, _ in tiles) + pad * 2
    rows = [[0x60, 0x60, 0x60] * width for _ in range(height)]

    x_offset = pad
    for size, zoom, pixels in tiles:
        for y in range(size * zoom):
            for x in range(size * zoom):
                r, g, b, a = pixels[(y // zoom) * size + (x // zoom)]
                if a == 0:
                    continue
                row = rows[pad + y]
                base = (x_offset + x) * 3
                row[base] = r
                row[base + 1] = g
                row[base + 2] = b
        x_offset += size * zoom + pad

    write_png(path, width, height, rows)

if __name__ == "__main__":
    main()
