#!/usr/bin/env python3
"""Generate the tray status icons and the app icon source.

Three states, one glyph: a filled dot in the menu bar.
  grey   ChatGPT is not running
  green  running with the debug port — the daemon can sync
  red    running without it — nothing can be synced

Written with zlib and struct so the build needs no image library.
"""

import struct
import zlib
from pathlib import Path

HERE = Path(__file__).resolve().parent

STATES = {
    "grey": (142, 142, 147),
    "green": (52, 199, 89),
    "red": (255, 59, 48),
}


def write_png(path: Path, size: int, pixel) -> None:
    raw = bytearray()
    for y in range(size):
        raw.append(0)  # filter: none
        for x in range(size):
            raw.extend(pixel(x, y))

    def chunk(tag: bytes, data: bytes) -> bytes:
        return (
            struct.pack(">I", len(data))
            + tag
            + data
            + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
        )

    header = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)  # 8-bit RGBA
    path.write_bytes(
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", header)
        + chunk(b"IDAT", zlib.compress(bytes(raw), 9))
        + chunk(b"IEND", b"")
    )


def dot(size: int, rgb: tuple[int, int, int], fill: float = 0.62):
    """A centred circle, supersampled 4x4 for smooth edges at menu-bar sizes."""
    centre = (size - 1) / 2
    radius = size * fill / 2

    def pixel(x: int, y: int) -> bytes:
        covered = 0
        for sy in range(4):
            for sx in range(4):
                dx = x + (sx + 0.5) / 4 - 0.5 - centre
                dy = y + (sy + 0.5) / 4 - 0.5 - centre
                if dx * dx + dy * dy <= radius * radius:
                    covered += 1
        return bytes((*rgb, round(255 * covered / 16)))

    return pixel


def main() -> None:
    for name, rgb in STATES.items():
        for scale, suffix in ((22, ""), (44, "@2x")):
            write_png(HERE / f"tray-{name}{suffix}.png", scale, dot(scale, rgb))
    # Source for `tauri icon`, which generates the .icns and Windows/Linux sets.
    write_png(HERE / "app-icon.png", 512, dot(512, STATES["green"], fill=0.72))
    print("wrote", ", ".join(sorted(p.name for p in HERE.glob("*.png"))))


if __name__ == "__main__":
    main()
