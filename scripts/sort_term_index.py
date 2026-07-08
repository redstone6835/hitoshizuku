#!/usr/bin/env python3
"""Sort docs/chapters/term-index.typ by Chinese pinyin.

The script only rewrites the term-index column block. Content after that block,
including the abbreviation table, is preserved byte-for-byte.
"""

from __future__ import annotations

import argparse
import difflib
import re
import string
import sys
from dataclasses import dataclass
from pathlib import Path

from pypinyin import Style, lazy_pinyin


DEFAULT_PATH = Path("docs/chapters/term-index.typ")


# pypinyin handles most cases well. Keep a few project-specific phrase
# overrides for polyphonic words that appear in OS terminology.
PHRASE_PINYIN = {
    "重命名": "chong ming ming",
    "行规程": "hang gui cheng",
    "终端行规程": "zhong duan hang gui cheng",
    "运行期": "yun xing qi",
    "运行状态": "yun xing zhuang tai",
    "运行资源限制": "yun xing zi yuan xian zhi",
    "运行资源用量": "yun xing zi yuan yong liang",
    "可执行与可链接格式": "ke zhi xing yu ke lian jie ge shi",
    "执行时关闭": "zhi xing shi guan bi",
    "执行替换": "zhi xing ti huan",
    "粘滞位": "zhan zhi wei",
}


ENTRY_RE = re.compile(
    r'#term-index-entry\(\s*"(?P<term>(?:[^"\\]|\\.)*)"\s*,\s*'
    r"\[(?P<gloss>.*?)\]\s*"
    r"(?:,\s*divider:\s*(?P<divider>true|false))?\s*"
    r"\)",
    re.S,
)


@dataclass(frozen=True)
class Entry:
    term: str
    gloss: str
    divider: str = "false"


def decode_typst_string(text: str) -> str:
    return text.replace(r"\"", '"').replace(r"\\", "\\")


def encode_typst_string(text: str) -> str:
    return text.replace("\\", r"\\").replace('"', r"\"")


def normalize_gloss(text: str) -> str:
    return re.sub(r"\s+", " ", text.strip())


def find_matching_paren(text: str, open_pos: int) -> int:
    depth = 0
    in_string = False
    escape = False
    for idx in range(open_pos, len(text)):
        ch = text[idx]
        if in_string:
            if escape:
                escape = False
            elif ch == "\\":
                escape = True
            elif ch == '"':
                in_string = False
            continue

        if ch == '"':
            in_string = True
        elif ch == "(":
            depth += 1
        elif ch == ")":
            depth -= 1
            if depth == 0:
                return idx

    raise ValueError("unmatched term-index-columns parenthesis")


def split_columns_block(text: str) -> tuple[str, str, str]:
    marker = "#term-index-columns("
    start = text.find(marker)
    if start < 0:
        raise ValueError("missing #term-index-columns block")
    open_pos = text.find("(", start)
    close_pos = find_matching_paren(text, open_pos)
    return text[:start], text[start : close_pos + 1], text[close_pos + 1 :]


def parse_entries(columns_block: str) -> list[Entry]:
    entries: list[Entry] = []
    for match in ENTRY_RE.finditer(columns_block):
        entries.append(
            Entry(
                term=decode_typst_string(match.group("term")),
                gloss=normalize_gloss(match.group("gloss")),
                divider=match.group("divider") or "false",
            )
        )
    if not entries:
        raise ValueError("no #term-index-entry entries found")
    return entries


def pinyin_words(term: str) -> list[str]:
    if term in PHRASE_PINYIN:
        return PHRASE_PINYIN[term].split()

    words = lazy_pinyin(
        term,
        style=Style.NORMAL,
        strict=False,
        errors=lambda chars: list(chars),
    )
    return [word.lower() for word in words if word.strip()]


def sort_key(entry: Entry) -> tuple[str, str]:
    words = pinyin_words(entry.term)
    pinyin = " ".join(words)
    return (pinyin, entry.term)


def group_letter(entry: Entry) -> str:
    words = pinyin_words(entry.term)
    if not words:
        return "#"
    first = words[0][0].upper()
    return first if first in string.ascii_uppercase else "#"


def render_entry(entry: Entry) -> str:
    return (
        f'      #term-index-entry("{encode_typst_string(entry.term)}", '
        f"[{entry.gloss}], divider: {entry.divider})"
    )


def render_columns(entries: list[Entry]) -> str:
    groups: dict[str, list[Entry]] = {}
    for entry in sorted(entries, key=sort_key):
        groups.setdefault(group_letter(entry), []).append(entry)

    lines = ["#term-index-columns(", "  ["]
    for group in sorted(groups):
        lines.append(f'    #term-index-group("{group}")[')
        for entry in groups[group]:
            lines.append(render_entry(entry))
        lines.append("    ]")
        lines.append("")
    if lines[-1] == "":
        lines.pop()
    lines.extend(["  ]", ")"])
    return "\n".join(lines)


def sort_term_index(text: str) -> str:
    prefix, columns_block, suffix = split_columns_block(text)
    entries = parse_entries(columns_block)
    return prefix + render_columns(entries) + suffix


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("path", nargs="?", default=DEFAULT_PATH, type=Path)
    parser.add_argument("--check", action="store_true", help="fail if sorting would change the file")
    parser.add_argument("--dry-run", action="store_true", help="print a unified diff instead of writing")
    args = parser.parse_args()

    original = args.path.read_text(encoding="utf-8")
    sorted_text = sort_term_index(original)

    if original == sorted_text:
        return 0

    diff = "".join(
        difflib.unified_diff(
            original.splitlines(keepends=True),
            sorted_text.splitlines(keepends=True),
            fromfile=str(args.path),
            tofile=str(args.path),
        )
    )

    if args.check:
        sys.stdout.write(diff)
        return 1

    if args.dry_run:
        sys.stdout.write(diff)
        return 0

    args.path.write_text(sorted_text, encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
