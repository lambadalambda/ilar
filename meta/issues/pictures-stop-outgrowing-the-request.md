# Pictures stop outgrowing the request

## Summary

A gateway chat that generated and looked at 47 pictures held 108 MB
of base64 images, all re-sent on every request, until Lemonade's
router refused the body at its 100 MB cap (HTTP 413) and the chat was
stuck. Two causes: images were stored at full size as PNG, and
nothing bounded what a request carries.

## Requirements

- On the way in, a PNG is fitted to 1024 px on the long side and
  stored as a JPEG when it has no transparency and that is smaller;
  other formats pass through.
- A recorded session event, `image_cutoff`, drops the pictures before
  a canonical index from requests while the text stays; the turn loop
  writes one when the images past the last cutoff exceed 24 MB,
  keeping the newest four, so the prefix a provider caches changes
  once per cutoff rather than on every turn.

## Acceptance Criteria

- Tests: a photo-like oversized PNG becomes a 1024-px JPEG, a
  transparent one a fitted PNG; the cutoff lands before the newest
  few images once the budget is crossed; a transcript drops pictures
  before the cutoff and keeps the words.
- The stuck chat on tenco answers again without `/new`.

## Notes

- Done 2026-09-11. Lemonade's cap is compiled in (cpp-httplib) with
  no config key; measured at exactly 100 MB.
