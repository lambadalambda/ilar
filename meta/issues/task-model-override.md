# Per-task model override + models tool

## Summary

Tasks could only choose among agent definitions; the calling model could
not pick a model per invocation, and had no way to see what models are
available or what they cost.

## Resolution

Task tool gains `model` and `reasoning` (per-call > agent definition >
inherit parent model+variant); explicit picks validated against the
session's available models and provider config. New read-only `models`
tool lists id, context window, pricing (or plan/free), and reasoning
variants. Shipped with spawn-path tests.
