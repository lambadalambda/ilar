# Two serve tests flake

## Summary

`serve::drive::tests::adoption_requeues_outbox_completions_as_follow_up_turns`
intermittently hangs until its own 30s patience runs out ("the
transcript never arrived") instead of failing an assertion. The
recovered completion's follow-up turn simply never happens. Measured on
2026-08-30 at roughly 3 failures in 27 runs, single-threaded, on an
otherwise idle machine.

It was seen once before, in the wave-2 full suite run (2026-08-29),
with all of that day's touched files unrelated to it.

## What has been ruled out

Three arms, same machine, `--test-threads=1`:

- current tree: **3 failures / 27**
- `serve/drive.rs` reverted to its pre-delivery-engine form, rest of the
  tree unchanged: **0 / 18**
- current tree with `outbox::lock` disabled: **0 / 18**

Neither difference is significant at these sample sizes (Fisher ≈ 0.07
for the pooled controls), and the `drive.rs` edit under suspicion —
cloning the notification out of the parcel instead of moving it — is
semantically identical to what it replaced. So this reads as a timing
race whose probability moves with codegen, not as a regression either
change introduced. It is recorded rather than attributed.

## A second one, same class

`serve.rs::the_listing_carries_a_row_per_root_session` fails its
`children.len() == 1` assertion — the children listing comes back empty
— at roughly 1 run in 19 (1 failure in 19 at the delivery-engine
commit, 0 in 6 at the commit before it). Same shape as the first: a
read that races whatever populates it, on a machine that has been
compiling for hours.

Neither flake is attributable to the delivery-engine batch, and neither
has a plausible mechanism in it: the batch touches the delivery rules,
not the listing cache and not the adoption pump. Both are recorded so
the next person to see one has the numbers rather than a shrug.

## Requirements

- Find where the follow-up turn is lost: the adoption's `pending`
  read, the requeue into the engine's queue, or the gate that decides a
  recovered completion may start a turn.
- Find what the listing reads before it is populated, and make the test
  wait for it rather than assume it.
- A failing run must fail *loudly* — an assertion about what did not
  happen — rather than by exhausting a poll loop, so the next
  occurrence names its own cause.

## Acceptance Criteria

- 100 consecutive single-threaded runs of both tests pass.
- The patience loop reports what it last saw when it gives up.

## Notes

- Parked with `ilar serve` — the test only builds under
  `--features serve`, and the driver it exercises is dormant. See
  [[serve-steps-out-of-the-default-build]].
- The outbox lock arm is worth re-running when this is picked up: a
  blocking `flock` taken from a tokio worker (`record`) against one
  taken from `spawn_blocking` (`pending`) is the kind of thing that
  shows up as a hang, and 0/18 is not proof of innocence.
