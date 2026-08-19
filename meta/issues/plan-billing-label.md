# Plan-billing cost label

## Summary

Coding-plan models show tokens with no dollars, indistinguishable from
"pricing missing".

## Requirements

- Coding-plan-only models (catalog access ZaiCodingPlan) display `plan`
  where the dollar figure would be, in status and the usage breakdown.
- Unknown/custom models keep the current tokens-only display.

## Acceptance Criteria

- Tests for the label selection.
