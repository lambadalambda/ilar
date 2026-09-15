# Tool errors share one shape

## Summary

What the model and the × row read, across crates/ilar/src/tools/:

- Shape is one per tool: `bash: …`, `read src: …`, `glob: …`,
  `webfetch invalid URL: …` prefixed; `old_string is empty; …`
  (edit.rs:106), `read: limit is at least 1`, `service start
  requires name and command`, `websearch query must not be empty`,
  `invalid todo status …`, `image_gen needs a prompt`, `unknown
  speaker …`, `no such tool: x`, `cancelled` unprefixed; task alone
  capitalised ("Subagent nesting limit reached (…)."). Five tools
  hand-roll `invalid input for X: {e}` instead of `parse_input`
  (web.rs:461,618; todo.rs:137; skill.rs:266; subagent.rs:2864,2984).
  One shape: `tool: message` or `tool path: message`, lowercase.
- websearch with no key fails as `websearch: exa HTTP 429 Too Many
  Requests` (web.rs:661-664, 834) and never names
  `ILAR_TAVILY_API_KEY` / `ILAR_EXA_API_KEY`, which
  docs/configuration.md:62-68 calls the fix.
- webfetch cuts at `…(truncated at 60000 chars)` with no offset and
  no spill (web.rs:503-507), and "response too large (limit 2097152
  bytes)" names no next step, while bash and grep spill through
  `SpillTarget` on the same ctx.
- grep's bare `…(truncated)` (grep.rs:449-451) covers three caps
  (50 matches/file, 2 MiB/file, 256 KiB output) none of which is in
  the schema; glob and grep's own limit cap name theirs.
- Schemas disagree about paths: read says "relative to cwd"
  (absolute works), write "relative to cwd, or absolute"; edit's four
  fields have no descriptions at all (edit.rs:90-93).
- bash and sudo describe the same `preview_bytes` and `timeout_ms`
  differently (bash.rs:458 vs sudo.rs:99; the timeout refusal at
  sudo.rs:120-122 lacks bash's hint); bash names "configured default
  for background" without the key.
- Timeouts: `bash: timed out after 120000ms` (bash.rs:643) and
  `600000ms` (subagent.rs:1670) where every row uses `2m 0s`;
  `websearch timed out` names no duration; the fixed 20 s web, 5 min
  image_gen and 10 s git probe timeouts are in no schema or doc.
- `no such tool: x` lists nothing; `bash: background runtime is
  unavailable` and `background mutation is unavailable inside a
  leased child workspace` give no next step.
- image_gen always says "The image is attached to this result"
  (image_gen.rs:253-257) though `with_images` may drop it and the
  chat wire may substitute "[image omitted]"; it never reads
  `ctx.vision` as read does.
- `human_bytes` gives `0 KiB` / `1 KiB` for tiny spills and `1
  lines` (bash.rs:119-123, 191-198); docs/interface.md:262 says
  "diffs for edits" where writes diff too.
- The `question` tool is undocumented in docs/interface.md (modal,
  Esc cancels, arrows, cancel is a non-error result); the exec
  refusal is "question capability is not attached to this registry"
  (turn.rs:2392) where the docs promise "is told so".

Size: S, many small edits. Source: UX sweep 2026-09-15, tools.
