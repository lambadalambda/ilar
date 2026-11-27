// Text as data. The same rules the Preact renderer had, but the parse
// returns plain objects and the components render every value as a
// text node: nothing from a session ever reaches an HTML parser.
//
// Plain text first: fenced blocks, inline backticks, paired **bold**,
// bare http(s) links. Everything else stays as written — a lone asterisk
// is an asterisk, because guessing at markdown in a shell transcript is
// how a path turns into italics. A **…** pair opens and closes on one
// line against non-space, lazily, with a bound so one long unclosed
// line does not cost a scan per marker.

const INLINE = /`([^`\n]+)`|\*\*(\S(?:[^\n]{0,400}?\S)??)\*\*|(https?:\/\/[^\s<>"'`)\]]+)/g;
const TRAILING = /[.,;:!?]+$/;

/** Spans: {t:"text"|"code"|"link", v} and {t:"bold", spans}. Links are
 * http(s) only, so no javascript: URL reaches an href. */
export function inline(text) {
  const source = String(text ?? "");
  const out = [];
  let last = 0;
  const push = (v) => {
    if (!v) return;
    const prior = out[out.length - 1];
    if (prior && prior.t === "text") prior.v += v;
    else out.push({ t: "text", v });
  };
  for (const match of source.matchAll(INLINE)) {
    push(source.slice(last, match.index));
    if (match[1] !== undefined) out.push({ t: "code", v: match[1] });
    else if (match[2] !== undefined) out.push({ t: "bold", spans: inline(match[2]) });
    else {
      const url = match[3].replace(TRAILING, "");
      out.push({ t: "link", v: url });
      push(match[3].slice(url.length));
    }
    last = match.index + match[0].length;
  }
  push(source.slice(last));
  return out;
}

// Markers demand the space after them, and a marker opening a list has
// at most three columns of indent (markdown's rule): `#include`, `-42`
// and `*ptr` are not markdown. Inside an open list the cap comes off,
// which is what nests.
const HEADING = /^(#{1,6})[ \t]+(\S.*)$/;
const BULLET = /^([ \t]*)[-*+][ \t]+(\S.*)$/;
const ORDERED = /^([ \t]*)(\d{1,9})[.)][ \t]+(\S.*)$/;
// GFM's pipe table and nothing looser; what does not parse stays text.
const TABLE_ROW = /^ {0,3}\|.*\|[ \t]*$/;

const indentColumns = (prefix) => prefix.replace(/\t/g, "    ").length;

const tableCells = (line) =>
  line
    .trim()
    .slice(1, -1)
    .split("|")
    .map((cell) => cell.trim());

// One alignment per column ("" left, "c", "r"), or null: not a separator.
function tableAligns(line) {
  if (!TABLE_ROW.test(line)) return null;
  const aligns = [];
  for (const cell of tableCells(line)) {
    const dashes = /^(:?)-+(:?)$/.exec(cell);
    if (!dashes) return null;
    aligns.push(dashes[1] && dashes[2] ? "c" : dashes[2] ? "r" : "");
  }
  return aligns;
}

// GFM's width rule: a row is as wide as its header.
function sizedRow(cells, width) {
  const row = cells.slice(0, width);
  while (row.length < width) row.push("");
  return row;
}

// A run of marker lines into one tree. Depth is relative: an item owns
// what follows it indented past it.
function listTree(items) {
  const nodes = [];
  let at = 0;
  while (at < items.length) {
    const item = items[at];
    const children = [];
    at += 1;
    while (at < items.length && items[at].indent > item.indent) {
      children.push(items[at]);
      at += 1;
    }
    nodes.push({ spans: inline(item.text), children: children.length ? listTree(children) : null });
  }
  return { kind: "list", ordered: items[0].ordered, start: items[0].start, items: nodes };
}

/** Blocks: text (spans), code (text), heading (level, spans), list
 * (ordered, start, items[{spans, children}]), table (header, aligns,
 * rows; cells are spans). */
export function blocks(text) {
  const lines = String(text ?? "").split("\n");
  const out = [];
  let buffer = [];
  let list = null;
  let fenced = null;
  // Prose and an open list are never both pending, which keeps `out` in
  // the order the text was written.
  const flush = () => {
    while (buffer.length && !buffer[0].trim()) buffer.shift();
    while (buffer.length && !buffer[buffer.length - 1].trim()) buffer.pop();
    const body = buffer.join("\n");
    if (body.trim()) out.push({ kind: "text", spans: inline(body) });
    buffer = [];
  };
  const flushList = () => {
    if (!list) return;
    out.push(listTree(list));
    list = null;
  };
  const close = () => {
    flushList();
    flush();
  };
  for (let at = 0; at < lines.length; at += 1) {
    const line = lines[at];
    const fence = /^\s*```/.test(line);
    if (fenced !== null) {
      if (fence) {
        out.push({ kind: "code", text: fenced.join("\n") });
        fenced = null;
      } else fenced.push(line);
      continue;
    }
    if (fence) {
      close();
      fenced = [];
      continue;
    }
    if (TABLE_ROW.test(line)) {
      const aligns = tableAligns(lines[at + 1] || "");
      const header = tableCells(line);
      if (aligns && aligns.length === header.length) {
        close();
        const rows = [];
        let next = at + 2;
        while (next < lines.length && TABLE_ROW.test(lines[next])) {
          rows.push(sizedRow(tableCells(lines[next]), header.length).map(inline));
          next += 1;
        }
        out.push({ kind: "table", header: header.map(inline), aligns, rows });
        at = next - 1;
        continue;
      }
    }
    const heading = HEADING.exec(line);
    if (heading) {
      close();
      out.push({ kind: "heading", level: Math.min(heading[1].length, 4), spans: inline(heading[2]) });
      continue;
    }
    const bullet = BULLET.exec(line);
    const ordered = bullet ? null : ORDERED.exec(line);
    if (bullet || ordered) {
      const indent = indentColumns(bullet ? bullet[1] : ordered[1]);
      if (list || indent <= 3) {
        const wanted = Boolean(ordered);
        if (list && list[0].ordered !== wanted && indent <= list[0].indent) flushList();
        flush();
        if (!list) list = [];
        list.push({
          text: bullet ? bullet[2] : ordered[3],
          indent,
          ordered: wanted,
          start: ordered ? Number(ordered[2]) : 1,
        });
        continue;
      }
    }
    if (list) {
      // An indented line under an item is the item, wrapped.
      if (/^[ \t]/.test(line) && line.trim()) {
        list[list.length - 1].text += " " + line.trim();
        continue;
      }
      flushList();
    }
    buffer.push(line);
  }
  if (fenced !== null) out.push({ kind: "code", text: fenced.join("\n") });
  close();
  return out;
}

// A first line cut inside a **bold** span keeps an orphan marker; drop
// it and keep the words.
function unpaired(cut) {
  const marks = cut.match(/\*\*/g);
  if (!marks || marks.length % 2 === 0) return cut;
  const at = cut.lastIndexOf("**");
  return cut.slice(0, at) + cut.slice(at + 2);
}

/** The first non-blank line, cut to `width`. */
export function preview(text, width = 90) {
  const line = String(text ?? "").split("\n").find((one) => one.trim()) || "";
  return unpaired(line.length > width ? line.slice(0, width - 1) + "…" : line);
}

/** Program output: a diff if it reads like one. Never markdown. */
export function looksLikeDiff(text) {
  if (/^(@@ |diff --git |--- |\+\+\+ )/m.test(text)) return true;
  const lines = text.split("\n");
  const added = lines.filter((line) => /^\+(?!\+)/.test(line)).length;
  const removed = lines.filter((line) => /^-(?!-)/.test(line)).length;
  return added > 0 && removed > 0 && added + removed >= lines.length * 0.3;
}

export function diffKind(line) {
  if (line.startsWith("@@")) return "hunk";
  if (/^\+(?!\+\+)/.test(line)) return "add";
  if (/^-(?!--)/.test(line)) return "del";
  return "ctx";
}
