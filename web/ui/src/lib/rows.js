// A log as rows to draw: plain descriptors, rebuilt each paint, keyed by
// the event's id or its canonical index so "load earlier" (which shifts
// every array position) keeps open rows open.

/** The compaction cut, at render time only: the whole array stays in
 * memory because a rewind's `to` indexes it. `kept_from` indexes the
 * whole log while `events` starts at `base`. */
export function compactionCut(events, base) {
  let cut = 0;
  events.forEach((event, index) => {
    if (event.type === "compaction") cut = Math.max(cut, Math.min((event.kept_from || 0) - base, index));
  });
  return Math.max(0, cut);
}

/** Row kinds: user, note, assistant, thinking, failed, tool, task,
 * result, model, divider. */
export function eventRows(events, offset) {
  const results = new Map();
  for (const event of events) if (event.type === "tool_result") results.set(event.tool_use_id, event);
  const consumed = new Set();
  const rows = [];
  events.forEach((event, index) => {
    const key = event.id || "@" + (offset + index);
    switch (event.type) {
      case "user_message":
        rows.push(
          event.notification
            ? { kind: "note", key, note: event.notification }
            : { kind: "user", key, event },
        );
        break;
      case "assistant_message":
        (event.content || []).forEach((block, at) => {
          const inner = key + ":" + at;
          if (block.type === "text") rows.push({ kind: "assistant", key: inner, text: block.text });
          else if (block.type === "reasoning_summary") rows.push({ kind: "thinking", key: inner, text: block.text });
          else if (block.type === "diagnostic") rows.push({ kind: "failed", key: inner, text: block.text });
          else if (block.type === "tool_call") {
            const result = results.get(block.id);
            if (result) consumed.add(result.id);
            rows.push({ kind: block.name === "task" ? "task" : "tool", key: inner, call: block, result });
          }
        });
        break;
      case "tool_result":
        // Only a result whose call is off the top of this page.
        if (!consumed.has(event.id)) rows.push({ kind: "result", key, event });
        break;
      case "model_change":
        rows.push({
          kind: "model",
          key,
          text: "model → " + event.model + (event.variant ? " · " + event.variant : ""),
        });
        break;
      case "compaction":
        rows.push({ kind: "thinking", key, label: "compacted", text: event.summary });
        break;
      case "rewind":
        rows.push({ kind: "divider", key, text: "rewound to event " + event.to });
        break;
      case "turn_ended":
        rows.push({ kind: "divider", key, text: event.detail });
        break;
      case "memory_recall":
        rows.push({ kind: "thinking", key, label: "recalled " + event.ids.length + " from memory", text: event.text });
        break;
      default:
        // meta, checkpoint, topic, subagent_invocation: state, not transcript.
        break;
    }
  });
  return rows;
}

export function pageRows(events, cursor) {
  const cut = compactionCut(events, cursor);
  return eventRows(events.slice(cut), cursor + cut);
}

const sameRow = (a, b) => {
  const keys = Object.keys(b);
  return keys.length === Object.keys(a).length && keys.every((key) => a[key] === b[key]);
};

/** Rows are rebuilt each paint; a row whose fields are the same objects
 * as last time is the last row itself, so the UI skips it. */
export function stabilize(previous, rows) {
  const cache = new Map();
  const kept = rows.map((row) => {
    const prior = previous.get(row.key);
    const row_ = prior && sameRow(prior, row) ? prior : row;
    cache.set(row.key, row_);
    return row_;
  });
  return { rows: kept, cache };
}

/** A session is live while a turn runs on it; undefined when nobody said. */
export function sessionLive(session) {
  return session && session.state ? session.state !== "idle" : undefined;
}

/** A call nothing will answer: the server swept it, or the session is
 * not running now (a tab open while the process was killed). */
export function unanswerable(call, live) {
  return call.state === "failed" || live === false;
}

export function toolState(call, result, live) {
  if (result) return result.is_error ? "err" : "ok";
  return unanswerable(call, live) ? "err" : "run";
}

export const UNANSWERED = "no result — the session stopped before this tool finished";

/** The running step, from delta frames: never folded into the events,
 * dropped when the committed step arrives. */
export function liveApply(previous, data) {
  if (data.type === "reset") return null;
  if (data.type === "turn_started") return { text: "", thoughts: [], tools: [] };
  const live = previous || { text: "", thoughts: [], tools: [] };
  if (data.type === "text_delta") live.text += data.text;
  else if (data.type === "thinking_delta") {
    if (!live.thoughts.length) live.thoughts.push("");
    live.thoughts[live.thoughts.length - 1] += data.text;
  } else if (data.type === "thinking_break") {
    if (live.thoughts.length && live.thoughts[live.thoughts.length - 1].trim()) live.thoughts.push("");
  } else if (data.type === "tool_started") {
    live.tools.push({ id: data.id, name: data.name, summary: data.summary });
  } else if (data.type === "tool_finished") {
    const tool = live.tools.find((one) => one.id === data.id);
    if (tool) tool.ok = data.ok;
  }
  return live;
}

/** A newer window of a child slice onto the one on screen; a window that
 * cannot be reconciled (a rewind, a compaction) replaces it. */
export function mergeSlice(current, fresh) {
  if (!current) return fresh;
  const keep = fresh.cursor - current.cursor;
  if (keep < 0 || keep > current.events.length) return fresh;
  return {
    ...fresh,
    events: current.events.slice(0, keep).concat(fresh.events),
    cursor: current.cursor,
    has_more: current.has_more,
  };
}

/** The detail panel's reads, in one pass. */
export function digest(events) {
  let meta = null;
  let latest = null;
  let compactions = 0;
  for (const event of events) {
    if (event.type === "assistant_message") latest = event;
    else if (event.type === "compaction") compactions += 1;
    else if (event.type === "meta" && !meta) meta = event;
  }
  return { meta: meta || {}, latest, compactions };
}

/** The model the session runs on now: its newest switch, else what the
 * session header says. */
export function currentModel(events, fallback) {
  for (let at = events.length - 1; at >= 0; at -= 1) {
    const event = events[at];
    if (event.type === "model_change") return event.model + (event.variant ? " · " + event.variant : "");
  }
  return fallback || "";
}

/** Sessions by directory, newest group first, no-directory last. */
export function grouped(sessions) {
  const groups = new Map();
  for (const session of sessions) {
    const key = session.cwd || "";
    if (!groups.has(key)) groups.set(key, []);
    groups.get(key).push(session);
  }
  const when = (session) => Date.parse(session.modified) || 0;
  const ordered = [];
  for (const [cwd, rows] of groups) {
    rows.sort((left, right) => when(right) - when(left));
    ordered.push({ cwd, rows, newest: when(rows[0]) });
  }
  ordered.sort((left, right) => (left.cwd === "") - (right.cwd === "") || right.newest - left.newest);
  return ordered;
}
