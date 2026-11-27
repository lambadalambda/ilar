import { test } from "node:test";
import assert from "node:assert/strict";
import { compactionCut, liveApply, mergeSlice, pageRows, toolState } from "../src/lib/rows.js";
import { fold } from "../src/lib/transcript.js";

const call = (id, name = "bash") => ({ type: "tool_call", id, name, summary: "ls" });

test("a tool call carries its result, and an orphan result gets its own row", () => {
  const events = [
    { type: "user_message", id: "u", text: "hi" },
    { type: "assistant_message", id: "a", content: [{ type: "text", text: "ok" }, call("c1"), call("t", "task")] },
    { type: "tool_result", id: "r1", tool_use_id: "c1", text: "out" },
    { type: "tool_result", id: "r9", tool_use_id: "gone", text: "orphan" },
    { type: "meta", id: "m" },
  ];
  const rows = pageRows(events, 0);
  assert.deepEqual(
    rows.map((r) => r.kind),
    ["user", "assistant", "tool", "task", "result"],
  );
  assert.equal(rows[2].result.text, "out");
  assert.equal(rows[3].result, undefined);
  assert.equal(new Set(rows.map((r) => r.key)).size, rows.length);
});

test("rows before a compaction's kept_from are cut, keys stay canonical", () => {
  const events = [
    { type: "user_message", text: "old" },
    { type: "user_message", text: "kept" },
    { type: "compaction", summary: "s", kept_from: 11 },
  ];
  assert.equal(compactionCut(events, 10), 1);
  const rows = pageRows(events, 10);
  assert.equal(rows[0].event.text, "kept");
  assert.equal(rows[0].key, "@11");
});

test("a tool with no result runs, unless the session stopped", () => {
  assert.equal(toolState({}, { is_error: true }, true), "err");
  assert.equal(toolState({}, undefined, true), "run");
  assert.equal(toolState({}, undefined, false), "err");
  assert.equal(toolState({ state: "failed" }, undefined, undefined), "err");
});

test("the live step gathers thoughts, text and tools; reset clears it", () => {
  let live = null;
  for (const d of [
    { type: "thinking_delta", text: "a" },
    { type: "thinking_break" },
    { type: "thinking_delta", text: "b" },
    { type: "text_delta", text: "hi" },
    { type: "tool_started", id: "1", name: "read", summary: "x" },
    { type: "tool_finished", id: "1", ok: true },
  ])
    live = liveApply(live, d);
  assert.deepEqual(live.thoughts, ["a", "b"]);
  assert.equal(live.text, "hi");
  assert.equal(live.tools[0].ok, true);
  assert.equal(liveApply(live, { type: "reset" }), null);
});

test("the fold applies each line once, and a rewind truncates within the window", () => {
  const view = { events: [], base: 0, line: 0, rewound: null };
  assert.equal(fold(view, "append", { line: 1, event: { n: 1 } }), "applied");
  assert.equal(fold(view, "append", { line: 1, event: { n: 1 } }), "stale");
  fold(view, "append", { line: 2, event: { n: 2 } });
  assert.equal(fold(view, "rewind", { line: 3, to: 1 }), "applied");
  assert.equal(view.events.length, 1);
  assert.equal(view.rewound, 1);
  view.base = 5;
  assert.equal(fold(view, "rewind", { line: 4, to: 2 }), "gone");
});

test("a fresh child slice keeps the head loaded earlier", () => {
  const current = { cursor: 0, events: [1, 2, 3], has_more: false };
  const fresh = { cursor: 2, events: [3, 4], has_more: true };
  assert.deepEqual(mergeSlice(current, fresh).events, [1, 2, 3, 4]);
  assert.equal(mergeSlice(current, { cursor: 9, events: [] }).cursor, 9);
});

test("stabilize keeps the row objects that did not change", async () => {
  const { stabilize } = await import("../src/lib/rows.js");
  const event = { type: "user_message", id: "u", text: "hi" };
  const first = stabilize(new Map(), pageRows([event], 0));
  const again = stabilize(first.cache, pageRows([event, { type: "rewind", id: "r", to: 0 }], 0));
  assert.equal(again.rows[0], first.rows[0]);
  assert.equal(again.rows.length, 2);
});
