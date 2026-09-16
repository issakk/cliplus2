"""Delimiter balance check for the Rust sources.

This machine has no toolchain — the build happens on CI — so the cheapest way to
catch the one edit that breaks every file after it, a replacement that ate a
closing brace, is to count delimiters here. Skips line comments, string literals
and char literals; it is not a parser and does not try to be.

    python tools/brace_check.py
"""

import pathlib
import re

PAIRS = {"{": "}", "(": ")", "[": "]"}
CLOSERS = {closer: opener for opener, closer in PAIRS.items()}

CHAR_LITERAL = re.compile(r"'(?:\\.|[^\\'])'")


def scan(text):
    """Yields `(line, char)` for every delimiter outside comments and literals."""
    index, length, line = 0, len(text), 1

    while index < length:
        char = text[index]

        if char == "\n":
            line += 1
        elif char == "/" and text[index + 1 : index + 2] == "/":
            newline = text.find("\n", index)
            if newline < 0:
                break
            line += 1
            index = newline + 1
            continue
        elif char == '"':
            index += 1
            while index < length:
                if text[index] == "\\":
                    index += 2
                    continue
                if text[index] == "\n":
                    line += 1
                if text[index] == '"':
                    break
                index += 1
        elif char == "'":
            literal = CHAR_LITERAL.match(text, index)
            if literal:
                index = literal.end()
                continue
        elif char in PAIRS or char in CLOSERS:
            yield line, char

        index += 1


def check(path):
    """Unclosed `(line, opener)` pairs, innermost last."""
    stack = []

    for line, char in scan(path.read_text(encoding="utf-8")):
        if char in PAIRS:
            stack.append((line, char))
        elif stack and stack[-1][1] == CLOSERS[char]:
            stack.pop()
        else:
            stack.append((line, char))  # a closer with nothing of its own to close

    return stack


def main():
    root = pathlib.Path(__file__).resolve().parent.parent / "rust" / "src"
    broken = 0

    for path in sorted(root.glob("*.rs")):
        unclosed = check(path)
        if not unclosed:
            continue

        broken += 1
        lines = path.read_text(encoding="utf-8").splitlines()
        print(f"{path.name}: {len(unclosed)} unclosed delimiter(s)")
        for line, char in unclosed:
            print(f"  {path.name}:{line} {char!r} {lines[line - 1].strip()[:80]}")

    if not broken:
        print("balanced")
    return 1 if broken else 0


if __name__ == "__main__":
    raise SystemExit(main())
