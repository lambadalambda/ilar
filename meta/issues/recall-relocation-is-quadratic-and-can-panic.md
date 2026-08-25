# Recall relocation is quadratic and can panic

## Summary

The case-insensitive match relocation in recall.rs:197-212 allocates
`entry.text[*index..].to_lowercase()` of the entire remaining suffix
for every candidate char index — O(n²) per entry, and
`search_sessions` runs per keystroke in the TUI search modal, so a
match near the end of a 100 KB tool-result entry does ~10⁵ full
lowercase allocations. Worse, the fallback
`unwrap_or(at.min(entry.text.len()))` uses `at`, a byte index into
the *lowercased* string; lowercasing can change byte lengths
('İ' → "i\u{307}"), so `excerpt()` can slice on a non-boundary and
panic.

## Requirements

- Relocate in one pass (e.g. walk both strings' char indices in
  lockstep) — no per-position suffix allocation.
- The fallback never uses a lowercased-string byte index against the
  original text.

## Acceptance Criteria

- A test with a Turkish-İ entry where the naive byte index is a
  non-boundary: no panic, sensible excerpt.

## Milestone

12 — Health sweep

## Outcome

`original_offset` maps lowercased-haystack offsets back in one
allocation-free walk of the original text, always landing on a real
boundary; both the match start AND end map through it — the end was
a second panic vector (`"i"` vs `"İstanbul"` sliced inside the
combining mark) the sweep hadn't named. O(n) per entry now. Pinned
by the Turkish-İ, exact-ASCII-offset, and lengthening-prefix tests.
(adc37fe)
