# The services panel shows what runs

## Summary

The sidebar's services panel lists services in raw order, capped at
four rows plus "+N more" — so with four exited services and one
running, the header says "services (1)" while the one running service
is hidden behind the cap. Worst of both worlds: dead services take the
space, the live one is invisible.

## Requirements

- Every *running* service is listed, uncapped — `carve_panel` already
  bounds the panel by available space, so no fixed row cap.
- Exited services are hidden, collapsed into one muted "N exited"
  line so a crashed service still registers.
- With nothing running, the panel is just that summary line.

## Acceptance Criteria

- A test with one running and five exited services shows the running
  one, no exited names, and "5 exited".

## Milestone

11 — Beyond the terminal

## Outcome

The panel lists every running service, uncapped — `carve_panel`
already bounds it by available sidebar space — and collapses exited
ones into a muted "N exited" line. The screenshot's case (one
running service invisible behind four dead rows and a "+2 more") now
renders as the running row plus "5 exited". Pinned by a render test.
