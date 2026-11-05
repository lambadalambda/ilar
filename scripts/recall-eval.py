#!/usr/bin/env python3
"""Measure memory recall's selector against labels a stronger model gives.

    recall-eval.py collect --sessions DIR --notes DIR --out DIR [--n 60]
    recall-eval.py label   --out DIR [--model gpt-6.1-sol]
    recall-eval.py select  --out DIR [--model gpt-6-luna]

`collect` samples prompts a person typed in root sessions, each with the
notes that existed when it was sent. `label` shows a strong model the
prompt, what the session did next and every note in full, and keeps the
notes it says would clearly help: silver labels, to be spot-checked.
`select` runs the selector as ilar does (its prompt is read from
crates/ilar/src/selector.rs; one line per note, never the body) and
reports precision, recall and how often it rightly says nothing.

Requests go through the ChatGPT login in ~/.local/state/ilar/auth.json.
The data is private — prompts and notes — so it stays in --out, outside
the repository.
"""

import argparse
import concurrent.futures
import glob
import json
import os
import random
import re
import time
import urllib.error
import urllib.request

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
AUTH = os.path.expanduser("~/.local/state/ilar/auth.json")
URL = "https://chatgpt.com/backend-api/codex/responses"
NOTIFICATIONS = ("<task-notification", "<tool-notification")


def selector_prompt():
    source = open(os.path.join(REPO, "crates/ilar/src/selector.rs")).read()
    text = re.search(r'pub const SELECTOR_PROMPT: &str = "(.*?)";', source, re.S).group(1)
    return re.sub(r"\\\n\s*", "", text).replace("\\n", "\n").replace('\\"', '"')


def ask(model, instructions, text, effort, key):
    auth = json.load(open(AUTH))
    body = {
        "model": model, "instructions": instructions, "stream": True, "store": False,
        "input": [{"type": "message", "role": "user",
                   "content": [{"type": "input_text", "text": text}]}],
        "tools": [], "prompt_cache_key": key, "reasoning": {"effort": effort},
    }
    headers = {
        "Authorization": "Bearer " + auth["access_token"], "Content-Type": "application/json",
        "originator": "codex_cli_rs", "OpenAI-Beta": "responses=experimental",
        "chatgpt-account-id": auth["account_id"], "session_id": key,
    }
    for attempt in range(4):
        try:
            out = ""
            request = urllib.request.Request(URL, data=json.dumps(body).encode(), headers=headers)
            for line in urllib.request.urlopen(request, timeout=300):
                if line.startswith(b"data:"):
                    try:
                        event = json.loads(line[5:])
                    except ValueError:
                        continue
                    if event.get("type") == "response.output_text.delta":
                        out += event["delta"]
            return out
        except (urllib.error.URLError, TimeoutError) as error:
            if attempt == 3:
                raise
            time.sleep(5 * (attempt + 1))


def parse_note(path):
    text = open(path).read()
    front, _, body = text.partition("\n---\n")
    fields = dict(re.findall(r"^(\w+):\s*(.*)$", front, re.M))
    return {**fields, "body": body.strip()}


def jsonl(path):
    return [json.loads(line) for line in open(path)] if os.path.exists(path) else []


def collect(args):
    notes = [parse_note(p) for p in glob.glob(os.path.join(args.notes, "*.md"))]
    notes.sort(key=lambda note: note["when"], reverse=True)
    cases = []
    for path in glob.glob(os.path.join(args.sessions, "*.jsonl")):
        events = [json.loads(line) for line in open(path)]
        meta = next((e for e in events if e["type"] == "meta"), {})
        if meta.get("parent_id"):
            continue
        for i, event in enumerate(events):
            text = event.get("text", "").strip()
            if event["type"] != "user_message" or text.startswith(NOTIFICATIONS):
                continue
            existing = [n for n in notes if n["when"] < event["ts"]]
            if len(existing) < args.min_notes:
                continue
            before = next((block.get("text", "") for e in reversed(events[:i])
                           if e["type"] == "assistant_message" for block in e["content"]
                           if block.get("type") == "text"), "")
            after = []
            for e in events[i + 1:]:
                if e["type"] == "user_message":
                    break
                if e["type"] == "assistant_message":
                    for block in e["content"]:
                        if block.get("type") == "text":
                            after.append(block["text"])
                        elif block.get("type") == "tool_call":
                            after.append(f"[{block['name']}] {json.dumps(block['input'])[:200]}")
            cases.append({"id": f"{os.path.basename(path)[:8]}-{i}", "prompt": text,
                          "before": before[-600:], "after": "\n".join(after)[:2000],
                          "notes": [n["id"] for n in existing]})
    random.seed(args.seed)
    cases = random.sample(cases, min(args.n, len(cases)))
    os.makedirs(args.out, exist_ok=True)
    with open(os.path.join(args.out, "notes.json"), "w") as f:
        json.dump({n["id"]: n for n in notes}, f)
    with open(os.path.join(args.out, "cases.jsonl"), "w") as f:
        for case in cases:
            f.write(json.dumps(case) + "\n")
    print(f"{len(cases)} cases, {len(notes)} notes -> {args.out}")


JUDGE = """You label which memory notes would have helped an AI agent with one message from \
the person it works for. You see the message, the end of the agent's previous reply, what the \
agent did next, and every note in full. A note helps when knowing it would have changed or sped \
up the agent's work on this message: a fact, decision, preference or fix it needed. A note that \
is only about the same project or topic does not help. Most messages are helped by no note; \
then answer with an empty list.
Answer with JSON only: {"helpful": ["note id", ...], "why": "one short sentence"}."""


def label(args):
    notes = json.load(open(os.path.join(args.out, "notes.json")))
    done = {row["id"] for row in jsonl(os.path.join(args.out, "labels.jsonl"))}
    todo = [case for case in jsonl(os.path.join(args.out, "cases.jsonl")) if case["id"] not in done]

    def one(case):
        listing = "\n\n".join(
            f"id: {nid}\nkind: {notes[nid].get('kind')}\ntitle: {notes[nid].get('title')}\n"
            f"summary: {notes[nid].get('summary')}\nbody: {notes[nid]['body'][:800]}"
            for nid in case["notes"])
        text = (f"# Notes\n\n{listing}\n\n# End of the agent's previous reply\n\n{case['before']}"
                f"\n\n# The message\n\n{case['prompt']}\n\n# What the agent did next\n\n{case['after']}")
        answer = ask(args.model, JUDGE, text, args.effort, f"recall-eval-label-{case['id']}")
        found = re.search(r"\{.*\}", answer, re.S)
        verdict = json.loads(found.group(0)) if found else {"helpful": [], "why": "unparsed"}
        helpful = [nid for nid in verdict.get("helpful", []) if nid in case["notes"]]
        return {"id": case["id"], "helpful": helpful, "why": verdict.get("why", "")}

    with concurrent.futures.ThreadPoolExecutor(args.jobs) as pool, \
            open(os.path.join(args.out, "labels.jsonl"), "a") as f:
        for row in pool.map(one, todo):
            f.write(json.dumps(row) + "\n")
            f.flush()
            print(row["id"], row["helpful"], row["why"][:100])


def age(now, when):
    days = (now - when).days
    return "today" if days == 0 else "yesterday" if days == 1 else (
        f"{days}d ago" if days < 30 else f"{days // 30}mo ago")


def strip(prompt):
    """What ilar's recall_query leaves of a prompt: no stamps, paths or URLs."""
    text = re.sub(r"<(now|task-notification|tool-notification)>.*?</\1>", " ", prompt, flags=re.S)
    words = []
    for token in text.split():
        if token.startswith(("/", "~", "\\")) or "://" in token:
            continue
        name = re.split(r"[/\\]", token)[-1]
        if any(c.isalpha() for c in name):
            words.append(name)
    return " ".join(words)


def select(args):
    from datetime import datetime
    notes = json.load(open(os.path.join(args.out, "notes.json")))
    labels = {row["id"]: row for row in jsonl(os.path.join(args.out, "labels.jsonl"))}
    cases = [case for case in jsonl(os.path.join(args.out, "cases.jsonl")) if case["id"] in labels]
    prompt = selector_prompt()
    parse = lambda s: datetime.fromisoformat(s.replace("Z", "+00:00"))

    def one(case):
        now = parse(max(notes[n]["when"] for n in case["notes"]))
        listing = "".join(
            f"{i + 1}. [{notes[n].get('kind')}] {age(now, parse(notes[n]['when']))} — "
            f"{notes[n].get('title')}: {notes[n].get('summary')}\n"
            for i, n in enumerate(case["notes"]))
        started = time.time()
        answer = ask(args.model, prompt, f"Notes:\n{listing}\n\nMessage:\n{strip(case['prompt'])}",
                     args.effort, f"recall-eval-select-{case['id'][:8]}")
        seconds = time.time() - started
        found = re.search(r"\{.*\}", answer, re.S)
        numbers = json.loads(found.group(0)).get("picks", []) if found else []
        picked = []
        for number in numbers:
            index = int(number) - 1 if str(number).isdigit() else -1
            if 0 <= index < len(case["notes"]) and case["notes"][index] not in picked:
                picked.append(case["notes"][index])
        return {"id": case["id"], "picked": picked[:5], "seconds": round(seconds, 2)}

    with concurrent.futures.ThreadPoolExecutor(args.jobs) as pool:
        results = list(pool.map(one, cases))
    with open(os.path.join(args.out, f"select-{args.model}-{args.effort}.jsonl"), "w") as f:
        for row in results:
            f.write(json.dumps(row) + "\n")
    hit = sum(len(set(r["picked"]) & set(labels[r["id"]]["helpful"])) for r in results)
    picked = sum(len(r["picked"]) for r in results)
    wanted = sum(len(labels[r["id"]]["helpful"]) for r in results)
    empty = [r for r in results if not labels[r["id"]]["helpful"]]
    quiet = sum(1 for r in empty if not r["picked"])
    seconds = sorted(r["seconds"] for r in results)
    print(f"{len(results)} cases, model {args.model}, effort {args.effort}")
    print(f"precision {hit}/{picked} = {hit / max(picked, 1):.0%}")
    print(f"recall    {hit}/{wanted} = {hit / max(wanted, 1):.0%}")
    print(f"quiet when nothing helps: {quiet}/{len(empty)}")
    print(f"latency median {seconds[len(seconds) // 2]}s, p90 {seconds[int(len(seconds) * 0.9)]}s")


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    sub = parser.add_subparsers(dest="command", required=True)
    c = sub.add_parser("collect")
    c.add_argument("--sessions", required=True)
    c.add_argument("--notes", required=True)
    c.add_argument("--out", required=True)
    c.add_argument("--n", type=int, default=60)
    c.add_argument("--min-notes", type=int, default=10)
    c.add_argument("--seed", type=int, default=1)
    for name, model in (("label", "gpt-6.1-sol"), ("select", "gpt-6-luna")):
        p = sub.add_parser(name)
        p.add_argument("--out", required=True)
        p.add_argument("--model", default=model)
        p.add_argument("--jobs", type=int, default=4)
        # What ilar sends: the model's lowest rung.
        p.add_argument("--effort", default="high" if name == "label" else "none")
    args = parser.parse_args()
    {"collect": collect, "label": label, "select": select}[args.command](args)


if __name__ == "__main__":
    main()
