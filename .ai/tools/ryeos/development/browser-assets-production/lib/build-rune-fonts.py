#!/usr/bin/env python3
# ryeos:signed:2026-11-13T13:05:39Z:712ca9ceed5f065f943fd3543ec025e912244ada9a782d0a50f1194d9a8ad55d:qzKnt4/eegUFBOmbPYUUdUbSpv8SNyvZACFewq5YdDcBccS1acqX/Mz1sn9NAexPfFrcuyTdVniwPGSnqzHUDw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# Externally invoked host helper in the development bundle; lib/ is excluded
# from Tool resolution, so this is signed source, not an admitted Tool. It
# grants no RyeOS authority. Run it from a checkout of this repository.
"""Produce the browser client's coverage fonts from pinned upstream Noto sources.

On the web Rune draws only the faces it is given: Inter and IBM Plex Mono cover
the interface, and these two cover content in other scripts.

  ryeos-cjk.ttf    Noto Sans SC at regular weight: the common Chinese characters
                   (GB 2312 level 1), the common Japanese kanji (JIS X 0208
                   level 1), kana, CJK punctuation and fullwidth forms.
  ryeos-emoji.ttf  Noto Emoji (monochrome outlines) at regular weight, whole.

    build-rune-fonts.py --sources DIR

DIR holds the upstream files under the names below (fetch them from the URLs);
each is checked against its pinned digest. Output goes to
crates/clients/rune/web/fonts/, where build-rune-web.sh picks it up. Needs
fontTools.
"""

from __future__ import annotations

import argparse
import hashlib
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[6]
OUT = ROOT / "crates/clients/rune/web/fonts"

SOURCES = {
    "NotoSansSC[wght].ttf": (
        "https://github.com/google/fonts/raw/main/ofl/notosanssc/NotoSansSC%5Bwght%5D.ttf",
        "a3041811a78c361b1de50f953c805e0244951c21c5bd412f7232ef0d899af0da",
    ),
    "NotoEmoji[wght].ttf": (
        "https://github.com/google/fonts/raw/main/ofl/notoemoji/NotoEmoji%5Bwght%5D.ttf",
        "de6c18832938afc99caf132b39d6a30a19bac7f2e812e28db2535b4608d27551",
    ),
}


def double_byte(codec: str, leads: range) -> set[int]:
    """Every character a two-byte EUC codec decodes from `leads` rows."""
    found: set[int] = set()
    for lead in leads:
        for trail in range(0xA1, 0xFF):
            try:
                found.update(map(ord, bytes([lead, trail]).decode(codec)))
            except UnicodeDecodeError:
                pass
    return found


def cjk_coverage() -> set[int]:
    # GB 2312 level 1 is rows 16-55; JIS X 0208 level 1 kanji are rows 16-47.
    covered = double_byte("gb2312", range(0xB0, 0xD8)) | double_byte("euc_jp", range(0xB0, 0xD0))
    for first, last in ((0x3000, 0x30FF), (0xFF00, 0xFFEF)):
        covered.update(range(first, last + 1))
    return covered


def build(source: Path, unicodes: set[int] | None, out: Path) -> None:
    from fontTools import subset
    from fontTools.ttLib import TTFont
    from fontTools.varLib import instancer

    # No save-time timestamp, so the same sources give the same bytes.
    font = TTFont(source, recalcTimestamp=False)
    font = instancer.instantiateVariableFont(font, {"wght": 400}, updateFontNames=True)
    options = subset.Options()
    options.hinting = False
    options.desubroutinize = True
    options.layout_features = ["*"]
    options.name_IDs = ["*"]
    options.notdef_outline = True
    subsetter = subset.Subsetter(options)
    subsetter.populate(unicodes=unicodes if unicodes is not None else set(font.getBestCmap()))
    subsetter.subset(font)
    font.save(out)
    print(f"{out.relative_to(ROOT)}: {out.stat().st_size:,} bytes")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--sources", type=Path, required=True)
    args = parser.parse_args()
    for name, (url, digest) in SOURCES.items():
        path = args.sources / name
        if not path.is_file():
            sys.exit(f"missing {path}: fetch {url}")
        actual = hashlib.sha256(path.read_bytes()).hexdigest()
        if actual != digest:
            sys.exit(f"{name} digest {actual} is not the pinned {digest}")
    OUT.mkdir(parents=True, exist_ok=True)
    build(args.sources / "NotoSansSC[wght].ttf", cjk_coverage(), OUT / "ryeos-cjk.ttf")
    build(args.sources / "NotoEmoji[wght].ttf", None, OUT / "ryeos-emoji.ttf")


if __name__ == "__main__":
    main()
