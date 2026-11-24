# Muse Spark rejects a long image session

## Summary

The millenia session (`70c302f6-60f0-4095-8e6b-d2a48d4ad664`) began returning an opaque HTTP 400 from `muse-spark-1.3-contributor` after a screenshot read. Retries fail with the same error.

## Requirements

- Identify the history-dependent request condition that causes the rejection.
- Preserve the saved session and avoid destructive recovery.
- Add a regression test if a client defect is confirmed.

## Acceptance criteria

- Explain the failure using session evidence or a controlled request comparison.
- Verify any fix with targeted tests.

## Notes

- The last successful response reported roughly 341k input tokens, below the catalog's 1M context limit.
- The session includes compaction and many screenshot results; raw image totals are not the live request totals.
- Controlled live probes reconstructed the post-compaction history (not the exact runtime request: system prompt and tool definitions omitted, output capped at 32). Full history: HTTP 400. Previous call/result pair omitted: HTTP 200. Only oldest image omitted: HTTP 200. Only newest image omitted: HTTP 200. All images omitted: HTTP 200. Opaque reasoning omitted: still HTTP 400.
- All 51 active images were accepted in isolation, both together and as separate user messages. Synthetic histories with 51 image tool results were accepted too. A simple 50-image cap is therefore not established.
- The reconstructed full request was about 10.6 MB, with 1,119 input items. The exact server constraint remains unknown; do not add a speculative model limit.
- Recovery: switch to another compatible model, compact, then return to Muse. Manual compaction on Muse replays the rejected history and may also fail. No session events or images were changed during diagnosis.
- Issue remains open. No source change or regression test yet; verification used controlled live requests.
