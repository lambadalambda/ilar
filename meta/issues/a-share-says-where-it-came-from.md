# A share says where it came from

## Summary

A shared file is meant to be sent to other people, and it carries the
author's working directory: `/Users/<name>/repos/<project>`, in the
session summary and again in the projected `meta` event. The panel
only shows the last component, so this is visible in the file rather
than on the page — but it is there, and it names the sender and what
they were working on.

The page does use the full path: `shorten()` trims it off the front of
paths in tool summaries, so dropping it outright makes those longer,
not shorter.

Not filed as a defect because it is a judgement call. A transcript is
full of the author's file paths anyway, and someone sharing one is
sharing their work. Whether a share should scrub that is the kind of
thing to decide once, deliberately.

## Requirements

- Decide whether a shared file carries the author's absolute paths.
- If not: the page still shortens tool-summary paths, which means the
  writer shortens them instead of the reader.

## Acceptance Criteria

- Whatever is decided, a test pins it, and
  `the_page_reaches_for_nothing` grows a case for absolute paths (it
  cannot see them today: `markup_only` strips the script bodies where
  the payload lives).

## Notes

- Raised reviewing the share feature, 2026-09-21.
- Size: S. Related: [[a-session-shares-as-one-html-file]].
