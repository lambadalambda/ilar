// Numbers and names as the TUI says them, so both surfaces agree.

/** "11h 46m": two units at most. */
export function age(iso) {
  const then = Date.parse(iso);
  if (Number.isNaN(then)) return "";
  const seconds = Math.max(0, (Date.now() - then) / 1000);
  if (seconds < 60) return Math.floor(seconds) + "s";
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return minutes + "m";
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return hours + "h " + (minutes % 60) + "m";
  const days = Math.floor(hours / 24);
  if (days < 30) return days + "d " + (hours % 24) + "h";
  return new Date(then).toISOString().slice(0, 10);
}

export function cost(dollars) {
  if (dollars >= 0.995) return "$" + dollars.toFixed(2);
  if (dollars >= 0.0005) return "$" + dollars.toFixed(3);
  return dollars > 0 ? "$<0.001" : "$0.00";
}

export const tokens = (count) => Number(count || 0).toLocaleString("en-US");

/** 63k, 1.2M. */
export function compact(count) {
  const value = Number(count || 0);
  if (value >= 1_000_000) return (value / 1_000_000).toFixed(value >= 10_000_000 ? 0 : 1) + "M";
  if (value >= 1_000) return Math.round(value / 1_000) + "k";
  return String(value);
}

export const basename = (path) => String(path || "").split("/").filter(Boolean).pop() || path || "";

/** A tool summary with the session's own directory taken off its paths. */
export function shorten(summary, cwd) {
  const text = String(summary || "");
  if (!cwd) return text;
  return text.split(cwd + "/").join("").split(cwd).join(".");
}

/** What a turn was holding: the wire's four counters. */
export function contextTokens(usage) {
  if (!usage) return 0;
  return (
    Number(usage.input || 0) +
    Number(usage.cache_read || 0) +
    Number(usage.cache_creation || 0) +
    Number(usage.output || 0)
  );
}

// One glyph per tool this binary ships; text, not an icon font.
const GLYPHS = {
  read: "▤",
  write: "✎",
  edit: "✎",
  bash: "▸",
  grep: "⌕",
  glob: "⌕",
  todo: "☑",
  task: "◆",
  tasks: "❖",
  service: "⚙",
  history: "↺",
  memory: "✦",
  models: "⇄",
  question: "?",
  webfetch: "◍",
  websearch: "⌕",
};

export function glyph(name) {
  const key = String(name || "").toLowerCase();
  return GLYPHS[key] || GLYPHS[key.split("_")[0]] || "•";
}

/** `apply_patch` is "Apply Patch". */
export function toolTitle(name) {
  return String(name || "tool")
    .split(/[_\-\s]+/)
    .filter(Boolean)
    .map((word) => word.charAt(0).toUpperCase() + word.slice(1))
    .join(" ");
}

export const message = (error) => String((error && error.message) || error);

/** What the page says about a write that worked. */
export const FATE_WORDS = {
  steering: "steering · next step",
  started: "turn started",
  aborted: "aborting …",
};
