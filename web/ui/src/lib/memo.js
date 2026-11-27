// A string-keyed cache for parses that a streaming turn would otherwise
// redo on every frame (the text cannot have changed while the string
// has not). Cleared whole when it grows, which is simpler than an LRU
// and cheap: a clear costs one re-parse per visible row.
export function memo(fn, limit = 4000) {
  const cache = new Map();
  return (text) => {
    const key = String(text ?? "");
    let value = cache.get(key);
    if (value === undefined) {
      if (cache.size >= limit) cache.clear();
      value = fn(key);
      cache.set(key, value);
    }
    return value;
  };
}
