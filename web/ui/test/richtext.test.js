import { test } from "node:test";
import assert from "node:assert/strict";
import { blocks, inline, preview } from "../src/lib/richtext.js";

test("inline finds code, paired bold and http links, and nothing else", () => {
  assert.deepEqual(inline("run `ls` on **this** at https://x.org/a. *ptr"), [
    { t: "text", v: "run " },
    { t: "code", v: "ls" },
    { t: "text", v: " on " },
    { t: "bold", spans: [{ t: "text", v: "this" }] },
    { t: "text", v: " at " },
    { t: "link", v: "https://x.org/a" },
    { t: "text", v: ". *ptr" },
  ]);
  assert.deepEqual(inline("**a** and **b**").filter((s) => s.t === "bold").length, 2);
  assert.deepEqual(inline("javascript:alert(1)"), [{ t: "text", v: "javascript:alert(1)" }]);
});

test("blocks parse fences, headings, nested lists and tables, prose stays prose", () => {
  const text = [
    "# Title",
    "intro",
    "- one",
    "  - nested",
    "- two",
    "",
    "```",
    "# not a heading",
    "```",
    "| a | b |",
    "|:-|-:|",
    "| 1 | 2 | 3 |",
    "#include <x>",
  ].join("\n");
  const out = blocks(text);
  assert.deepEqual(
    out.map((b) => b.kind),
    ["heading", "text", "list", "code", "table", "text"],
  );
  assert.equal(out[2].items[0].children.items[0].spans[0].v, "nested");
  assert.equal(out[3].text, "# not a heading");
  assert.deepEqual(out[4].aligns, ["", "r"]);
  assert.equal(out[4].rows[0].length, 2);
});

test("a wrapped bullet line joins its item; four spaces with no list is not one", () => {
  const out = blocks("- long\n  tail\n\n    - code");
  assert.equal(out[0].items[0].spans[0].v, "long tail");
  assert.equal(out[1].kind, "text");
});

test("preview takes the first non-blank line, cuts it and drops an orphan **", () => {
  assert.equal(preview("\n**Planning the thing** and more", 12), "Planning …");
  assert.equal(preview("short"), "short");
});
