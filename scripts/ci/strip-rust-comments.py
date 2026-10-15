#!/usr/bin/env python3
"""Blank Rust comments from stdin, keeping string and char literals intact.

`//` line comments and (nested) `/* */` block comments are replaced by
nothing / spaces; newlines are always kept so line numbers still match the
input. Text inside "..." and r#"..."# literals and real char literals is
preserved, so a `//` inside a URL string does not hide the rest of the line.
"""
import re
import sys

src = sys.stdin.read()
n = len(src)
out = []
i = 0
raw_open = re.compile(r'b?r(#*)"')
char_lit = re.compile(r"'(?:\\(?:x[0-9a-fA-F]{2}|u\{[0-9a-fA-F_]+\}|.)|[^\\'\n])'")
while i < n:
    c = src[i]
    nxt = src[i + 1] if i + 1 < n else ""
    prev = src[i - 1] if i else ""
    if c == "/" and nxt == "/":
        while i < n and src[i] != "\n":
            i += 1
    elif c == "/" and nxt == "*":
        depth = 1
        i += 2
        while i < n and depth:
            if src.startswith("/*", i):
                depth += 1
                i += 2
            elif src.startswith("*/", i):
                depth -= 1
                i += 2
            else:
                if src[i] == "\n":
                    out.append("\n")
                i += 1
        out.append(" ")
    else:
        m = raw_open.match(src, i) if c in "br" and not (prev.isalnum() or prev == "_") else None
        if m:
            end = src.find('"' + m.group(1), m.end())
            end = n if end < 0 else end + 1 + len(m.group(1))
            out.append(src[i:end])
            i = end
        elif c == '"':
            j = i + 1
            while j < n and src[j] != '"':
                j += 2 if src[j] == "\\" else 1
            out.append(src[i : j + 1])
            i = j + 1
        elif c == "'" and (m := char_lit.match(src, i)):
            out.append(m.group(0))
            i = m.end()
        else:
            out.append(c)
            i += 1
sys.stdout.write("".join(out))
