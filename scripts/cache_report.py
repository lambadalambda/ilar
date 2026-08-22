#!/usr/bin/env python3
"""Prompt-cache forensics over session JSONL.

Every `assistant_message` event carries the usage of exactly one provider
request, with a timestamp, so a session is already a complete record of
what the provider cached and when. This reads that record.

    scripts/cache_report.py <session-id-or-path>   # one session, request by request
    scripts/cache_report.py --all                  # what predicts a miss, across all sessions

A "miss" is a request that reported zero cached tokens on a prompt large
enough to be cache-eligible. Note what a miss cannot be: our prefix is
append-only, so a prefix that changed mid-session would still leave the
head cached and report a *partial* read. Zero on a large prompt is the
backend declining to match anything.
"""

import collections
import datetime
import glob
import json
import os
import sys

SESSIONS = os.path.expanduser("~/.local/state/ilar/sessions")
# OpenAI caches prefixes of at least 1024 tokens; below that a zero is
# expected rather than interesting.
ELIGIBLE_TOKENS = 5000


def load(path):
    events = []
    with open(path) as handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            try:
                events.append(json.loads(line))
            except json.JSONDecodeError:
                pass  # a torn tail line during a live session
    return events


def requests_of(events):
    """One row per provider call: the assistant messages, in order."""
    previous = None
    for event in events:
        if event.get("type") != "assistant_message":
            continue
        usage = event.get("usage", {})
        stamp = datetime.datetime.fromisoformat(event["ts"].replace("Z", "+00:00"))
        calls = [b.get("name") for b in event.get("content", []) if b.get("type") == "tool_call"]
        yield {
            "ts": stamp,
            "gap": (stamp - previous).total_seconds() if previous else None,
            "model": event.get("model", "?"),
            "input": usage.get("input_tokens", 0),
            "read": usage.get("cache_read_input_tokens", 0),
            "write": usage.get("cache_creation_input_tokens", 0),
            "output": usage.get("output_tokens", 0),
            "calls": calls,
        }
        previous = stamp


def prompt_tokens(row):
    return row["input"] + row["read"]


def eligible(row, index):
    return index > 1 and prompt_tokens(row) >= ELIGIBLE_TOKENS


def report_session(path):
    events = load(path)
    rows = list(requests_of(events))
    print(f"session {os.path.basename(path)}  ({len(rows)} provider requests)")
    print(f"{'#':>3} {'time':>8} {'gap':>6} {'prompt':>8} {'cached':>8} {'hit':>5} {'out':>6}  tools")
    missed = considered = 0
    for index, row in enumerate(rows, start=1):
        total = prompt_tokens(row)
        share = row["read"] / total if total else 0
        gap = f"{row['gap']:.0f}s" if row["gap"] is not None else "-"
        tools = ", ".join(f"{n}×{c}" for n, c in collections.Counter(row["calls"]).items())
        flag = ""
        if eligible(row, index):
            considered += 1
            if row["read"] == 0:
                missed += 1
                flag = "MISS"
        print(
            f"{index:>3} {row['ts']:%H:%M:%S} {gap:>6} {total:>8} {row['read']:>8}"
            f" {share:>4.0%} {row['output']:>6}  {flag} {tools}"
        )
    if considered:
        print(f"\n{missed}/{considered} cache-eligible requests read nothing ({missed / considered:.0%})")


def bucket(label_of, rows_by_session):
    counts = collections.defaultdict(lambda: [0, 0])
    for rows in rows_by_session:
        for index, row in enumerate(rows, start=1):
            if not eligible(row, index):
                continue
            label = label_of(row, index, rows)
            if label is None:
                continue
            counts[label][0] += 1
            counts[label][1] += 1 if row["read"] == 0 else 0
    return counts


def show(title, counts, order):
    print(f"\n{title}")
    for key in order:
        if key not in counts:
            continue
        total, misses = counts[key]
        print(f"  {key:>10}: {misses:>5}/{total:<6} {misses / total:>5.0%} miss")


def report_all():
    rows_by_session = []
    for path in glob.glob(os.path.join(SESSIONS, "*.jsonl")):
        rows = list(requests_of(load(path)))
        if rows:
            rows_by_session.append(rows)
    print(f"{len(rows_by_session)} sessions")

    show(
        "by provider",
        bucket(lambda row, i, rows: "zai" if row["model"].startswith("zai") else "openai", rows_by_session),
        ["openai", "zai"],
    )
    show(
        "by request index in the session",
        bucket(lambda row, i, rows: str(i) if i <= 6 else ("7-15" if i <= 15 else "16+"), rows_by_session),
        ["2", "3", "4", "5", "6", "7-15", "16+"],
    )

    def growth(row, index, rows):
        previous = rows[index - 2]
        grew = prompt_tokens(row) - prompt_tokens(previous)
        return "<2k" if grew < 2000 else "2-10k" if grew < 10000 else "10-30k" if grew < 30000 else ">30k"

    show(
        "by how much the prompt grew since the previous request",
        bucket(growth, rows_by_session),
        ["<2k", "2-10k", "10-30k", ">30k"],
    )

    def previous_calls(row, index, rows):
        count = len(rows[index - 2]["calls"])
        return "0" if count == 0 else "1-2" if count <= 2 else "3-5" if count <= 5 else "6+"

    show(
        "by tool calls in the previous step",
        bucket(previous_calls, rows_by_session),
        ["0", "1-2", "3-5", "6+"],
    )

    def gap(row, index, rows):
        seconds = row["gap"]
        if seconds is None:
            return None
        return "<15s" if seconds < 15 else "15-60s" if seconds < 60 else "1-5m" if seconds < 300 else ">5m"

    show("by gap since the previous request", bucket(gap, rows_by_session), ["<15s", "15-60s", "1-5m", ">5m"])


def main():
    argument = sys.argv[1] if len(sys.argv) > 1 else "--all"
    if argument == "--all":
        report_all()
        return
    path = argument if os.path.exists(argument) else os.path.join(SESSIONS, f"{argument}.jsonl")
    if not os.path.exists(path):
        sys.exit(f"no such session: {argument}")
    report_session(path)


if __name__ == "__main__":
    main()
