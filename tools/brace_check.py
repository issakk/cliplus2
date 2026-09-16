"""Delimiter and string-literal checks for the Rust sources.

This machine has no toolchain — the build happens on CI — so the cheapest way to
catch an anchor edit that ate half of a pair is to look for it here, before a push.
Two failure modes, both seen in practice:

* a closing brace or paren swallowed by a replacement, which makes every file after
  it fail to parse;
* one of the two quotes of a string literal swallowed, after which everything to
  the end of the file lexes as string contents and rustc reports nonsense near the
  end of unrelated literals.

    python tools/brace_check.py

Not a parser, and does not try to be: comments, string literals and char literals
are skipped by hand.
"""

import pathlib
import sys
import re

PAIRS = {"{": "}", "(": ")", "[": "]"}
CLOSERS = {closer: opener for opener, closer in PAIRS.items()}
CHAR_LITERAL = re.compile(r"'(?:\\.|[^\\'])'")


def tokens(text):
    """Yields `(line, token)` for delimiters, quotes and newlines.

    String *contents* are skipped but their quotes and newlines are yielded, which
    is what lets the checks below see where a literal starts and stops.
    """
    index, length, line = 0, len(text), 1

    while index < length:
        char = text[index]

        if char == "\n":
            yield line, "\n"
            line += 1
        elif char == "/" and text[index + 1 : index + 2] == "/":
            newline = text.find("\n", index)
            if newline < 0:
                break
            index = newline
            continue
        elif char == '"':
            yield line, '"'
            index += 1
            while index < length:
                if text[index] == "\\":
                    if text[index + 1 : index + 2] == "\n":
                        line += 1
                    index += 2
                    continue
                if text[index] == "\n":
                    yield line, "\n"
                    line += 1
                elif text[index] == '"':
                    yield line, '"'
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


def unclosed_delimiters(tokens_found):
    """`(line, char)` for every opener that never closed, innermost last."""
    stack = []

    for line, char in tokens_found:
        if char in PAIRS:
            stack.append((line, char))
        elif char == '"' or char == "\n":
            continue
        elif stack and stack[-1][1] == CLOSERS[char]:
            stack.pop()
        elif char in CLOSERS:
            stack.append((line, char))  # a closer with nothing of its own to close

    return stack


def string_start_of_eof(tokens_found):
    """The line an unterminated string literal starts on, if the file ends in one."""
    start = None

    for line, char in tokens_found:
        if char == '"':
            start = line if start is None else None

    return start


def main(argv):
    root = pathlib.Path(__file__).resolve().parent.parent / "rust" / "src"
    paths = [pathlib.Path(argument) for argument in argv] or sorted(root.glob("*.rs"))
    broken = 0

    for path in paths:
        found = list(tokens(path.read_text(encoding="utf-8")))
        lines = path.read_text(encoding="utf-8").splitlines()
        problems = []

        for line, char in unclosed_delimiters(found):
            problems.append(f"{path.name}:{line} unclosed {char!r}  {lines[line - 1].strip()[:70]}")

        if (line := string_start_of_eof(found)) is not None:
            problems.append(
                f"{path.name}:{line} string literal never closes  {lines[line - 1].strip()[:70]}"
            )

        if problems:
            broken += 1
            print("\n".join(problems))

    if not broken:
        print("clean")

    return 1 if broken else 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
