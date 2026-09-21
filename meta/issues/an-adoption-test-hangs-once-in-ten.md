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


## Note (2026-09-05)

`adoption_requeues_outbox_completions_as_follow_up_turns` failed two
gate runs in a row ("the transcript never arrived" after 30 s) and
passed 3/3 alone in 60 ms each time; `transcript_patiently` now waits
a minute. The underlying sensitivity to a loaded machine stands.

Later the same day, with tenco idle: `cargo test --workspace
--all-features --bin ilar` fails this test every time (at HEAD and at
cc9b9e8, whose full gate had passed that morning), while `cargo test -p
ilar-tui --all-features` passes it every time, as do the workspace
flags with any filter, and the workspace flags with
`--test-threads=1`. The enabled feature sets of every dependency are
identical between the two invocations (`cargo tree -e features`
diffed). So: the same binary, the same tests, parallel, and only the
workspace invocation starves it. Not understood. `scripts/check.sh`
now runs the TUI's all-features suite crate-scoped, which is the same
coverage without the interaction; the 30 s → 60 s patience change is
kept but was not the fix.

## And again (2026-09-21)

The retry below made the loss rare, not impossible: under the load of
two test loops on one box, a replay took longer than the gap between
a writer's appends and all five tries saw the file move — the pinning
test failed one run in six, on main. The scan's two liveness questions
("is the session there", "whose tree is it") are answered by the head
record now, which no append touches; the retry and its constants are
gone. The ancestry walk was the quieter half of the bug: a refused
replay there read as "another process's tree" and skipped the entry
without a word. Thirty runs green; then fifteen of fifteen beside the
old code looping on the same box, which managed nine.

## Found and fixed (2026-09-20)

It was not a test problem. It was a real one, and the test was the only
thing reporting it.

The patience loop was made to say what it last saw instead of only that
it gave up, and the first failure named the shape at once: the first
turn had run and answered, and the recovered completion had simply
never become a second turn. No drop message from `follow_up` — its slot
wait is patient and its lease wait says so when it gives up — which
left only one candidate: the adoption scan came back empty.

A line on the skipped-entry arm of `outbox::pending` confirmed it, the
same text every failing run:

    outbox: skipping <id>: its log could not be read
    (session <id>: session path changed during canonical replay)

`pending` reads each parent's log to decide what is undelivered. The
store refuses a window it saw change mid-read rather than hand back
half of one. `pending` took that refusal as "skip it, a later scan will
get it" — true for a surface that opens a session and scans once, and
false for `ilar serve`, whose adoption fires *at the moment a message
starts a turn on that very session*. The scan and the write are
concurrent by construction. There is no later scan: the engine adopts
once per session, at start.

So a recovered task result was lost for the life of the process,
whenever the race landed. The test was not flaky about nothing; it was
flaky about a real completion going missing.

The read is retried now — five attempts, 20 ms apart, and only for
errors that are not "this session is gone". The unreadable case still
skips, and now says so.

**Measured.** Before: 4/25 and 3/25 failures under `cargo test
--workspace --all-features --bin ilar` on tenco. After: **0 failures in
100 consecutive runs** of the same invocation.

The second flake — `the_listing_carries_a_row_per_root_session` coming
back with no children — was a test problem. The roots and the children
come off the same cache but not necessarily from the same refresh, so
waiting for the roots said nothing about the children. It waits for the
children now, and says how many it last saw if it gives up.

Single-threaded, the criterion as written: 0 failures in 50 runs of the
whole `serve::` set on tenco.
