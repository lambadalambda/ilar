#!/usr/bin/env python3
"""Reduce a module's `pub(crate)` markers to the minimum that compiles.

Every review of the main.rs split found the same defect: applying
visibility with a regex over-exports badly (46 of 113 in one seam). The
reliable method is to strip every marker and let the compiler name what
it actually needs, so do exactly that.

Usage: minimize_visibility.py <module.rs>
"""

import json
import re
import subprocess
import sys

# `pub(crate) ` only ever means a visibility qualifier at the start of a
# line; anywhere else it is text inside a string or comment.
STRIP = re.compile(r"^(\s*)pub\(crate\) ", re.M)

# Privacy diagnostics, by rustc error code.
PRIVACY = {"E0603", "E0616", "E0624", "E0446", "E0451", "E0492"}
NAME = re.compile(r"`(?:[\w:]*::)?(\w+)`")
# "field `shared` of struct `Beta` is private" — the struct matters: two
# structs can share a field name, and promoting the first match exports
# the wrong one, which is the very over-export this script exists to stop.
FIELD_OF = re.compile(r"field `(\w+)` of struct `(?:[\w:]*::)?(\w+)`")


def build():
    """Return (ok, names rustc says are too private)."""
    out = subprocess.run(
        ["cargo", "build", "--all-targets", "--message-format=json"],
        capture_output=True,
        text=True,
    ).stdout
    names, other = set(), []
    for line in out.splitlines():
        try:
            msg = json.loads(line).get("message")
        except json.JSONDecodeError:
            continue
        if not msg or msg.get("level") != "error":
            continue
        code = (msg.get("code") or {}).get("code")
        text = msg.get("message", "")
        privacy = code in PRIVACY or "is private" in text or "more private" in text
        if privacy:
            field = FIELD_OF.search(text)
            if field:
                names.add((field.group(2), field.group(1)))
            else:
                names |= set(NAME.findall(text))
        else:
            other.append(f"{code}: {text}")
    return other, names


STRUCT_BLOCK = re.compile(r"^(?:pub\(crate\) )?struct \w+[^\n]*\{\n(?:.*\n)*?\}", re.M)


def promote(source, names):
    """Add `pub(crate)` to each named item, or to a struct field.

    Fields are only promoted inside a `struct` block: the same
    `name: value` shape appears in struct *literals* inside function
    bodies, and promoting one of those is a syntax error.
    """
    for name in sorted(names, key=str):
        if isinstance(name, tuple):
            source = promote_field(source, *name)
            continue
        item = rf"^(\s*)((?:fn|struct|enum|static|const|type|trait) {name}\b)"
        new, count = re.subn(item, r"\1pub(crate) \2", source, count=1, flags=re.M)
        if count:
            source = new
            continue
    return source


def promote_field(source, struct, field):
    """Promote one field of one named struct."""
    block = re.search(
        rf"^(?:pub\(crate\) )?struct {struct}\b[^\n]*\{{\n(?:.*\n)*?\}}", source, re.M
    )
    if not block:
        return source
    fixed = re.sub(
        rf"^(\s+)({field}:\s)", r"\1pub(crate) \2", block.group(0), count=1, flags=re.M
    )
    return source[: block.start()] + fixed + source[block.end() :]


def main():
    path = sys.argv[1]
    original = open(path).read()
    open(path, "w").write(STRIP.sub(r"\1", original))

    for _ in range(60):
        errors, names = build()
        if not names:
            if errors:
                open(path, "w").write(original)
                sys.exit("non-privacy errors, restored:\n  " + "\n  ".join(errors[:5]))
            break
        source = open(path).read()
        promoted = promote(source, names)
        if promoted == source:
            open(path, "w").write(original)
            sys.exit(f"could not promote: {', '.join(sorted(names))}")
        open(path, "w").write(promoted)
    else:
        open(path, "w").write(original)
        sys.exit("did not converge")

    before = original.count("pub(crate)")
    after = open(path).read().count("pub(crate)")
    print(f"{path}: {before} -> {after} pub(crate)")


if __name__ == "__main__":
    main()
