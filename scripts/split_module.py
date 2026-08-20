#!/usr/bin/env python3
"""Move top-level items between Rust files, verbatim.

The TUI split is mechanical: every item in main.rs starts at column 0 and
ends at a column-0 `}` (or a `;` line). Chunk on that, move whole blocks
by name, and the moved text is guaranteed identical to what was there.

Usage: split_module.py <src.rs> <dst.rs> <name> [<name> ...]
Names are the item's identifier (fn/struct/enum/impl target/const/static).
`impl X` moves every `impl ... for X` / `impl X` block.
"""

import os
import re
import sys


def chunk(text):
    """Split into (kind, name, body) top-level blocks, preserving order."""
    lines = text.split("\n")
    blocks = []
    current = []
    depth = 0
    for line in lines:
        current.append(line)
        depth += line.count("{") - line.count("}")
        # A top-level item closes when braces balance and the line is a
        # column-0 close, or it's a one-line item ending in `;`.
        if depth == 0 and line and not line[0].isspace():
            if line.startswith("}") or line.rstrip().endswith(";"):
                blocks.append("\n".join(current))
                current = []
    if current:
        blocks.append("\n".join(current))
    verify(blocks)
    return blocks


def verify(blocks):
    """Fail loudly if a block holds more than one item.

    The depth counter is blind to braces inside strings, chars, raw
    strings and comments. A skewed count merges every following item into
    one block, and since `item_names` stops at the first match, the extras
    become invisible — asking for one name would silently drag them along.
    Under-moving already errors out; this makes over-moving just as loud.
    """
    for block in blocks:
        starts = [
            line
            for line in block.split("\n")
            if line and not line[0].isspace() and ITEM.match(line)
        ]
        if len(starts) > 1:
            names = ", ".join(s.strip() for s in starts[:4])
            sys.exit(
                "refusing to move: brace counting merged these into one "
                f"block, so the chunker is mis-reading the file — {names}"
            )


ITEM = re.compile(
    r"^(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?"
    r"(fn|struct|enum|trait|const|static|type|macro_rules!|impl|mod)\s+"
    r"([A-Za-z_][A-Za-z0-9_]*)",
    re.M,
)


def item_names(block):
    """Every identifier this block defines, for matching."""
    names = set()
    for line in block.split("\n"):
        if line.startswith("#") or line.startswith("//") or not line.strip():
            continue
        if line and line[0].isspace():
            continue
        m = ITEM.match(line)
        if m:
            kind, name = m.group(1), m.group(2)
            if kind == "impl":
                # `impl Foo`, `impl Trait for Foo`, `impl<T> Foo`
                tail = re.search(r"(?:for\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*(?:<[^>]*>)?\s*\{", line)
                if tail:
                    names.add(tail.group(1))
                names.add(name)
            else:
                names.add(name)
            break
    return names


def main():
    src_path, dst_path, *wanted = sys.argv[1:]
    wanted = set(wanted)
    src = open(src_path).read()
    blocks = chunk(src)

    moved, kept, seen = [], [], set()
    for block in blocks:
        names = item_names(block)
        if names & wanted:
            moved.append(block)
            seen |= names & wanted
        else:
            kept.append(block)

    missing = wanted - seen
    if missing:
        sys.exit(f"not found: {', '.join(sorted(missing))}")

    open(src_path, "w").write("\n".join(kept))
    # A block carries its own leading blank line. Appending to a non-empty
    # file consumes that blank as the newline closing the previous item,
    # silently welding the two together, so put it back.
    existing = open(dst_path).read() if os.path.exists(dst_path) else ""
    separator = "\n" if existing.strip() and not existing.endswith("\n\n") else ""
    with open(dst_path, "a") as out:
        out.write(separator + "\n".join(moved))
    print(f"moved {len(moved)} blocks -> {dst_path}")


if __name__ == "__main__":
    main()
