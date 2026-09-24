#!/usr/bin/env python3
"""What OpenCode Zen and Go list that the catalog does not, and the reverse.

Reads the live model listings (they carry ids only), the catalog in
crates/ilar/src/model.rs, and models.dev for everything a row needs:
limits, input kinds, effort ladder, reasoning field, prices, and a hint
for the wire (`@ai-sdk/openai` is the Responses wire). With --probe, each
candidate is sent a small request on that wire.

The key is OPENCODE_GO from the environment or ~/.env. Writing the rows
stays a judgement: see the notes above the OpenCode rows in model.rs.

    scripts/opencode_diff.py [--probe]
"""

import json
import os
import re
import sys
import urllib.error
import urllib.request
import uuid
from datetime import date
from pathlib import Path

GATEWAYS = {
    "opencode": "https://opencode.ai/zen/v1",
    "opencode-go": "https://opencode.ai/zen/go/v1",
}
CATALOG = Path(__file__).resolve().parent.parent / "crates/ilar/src/model.rs"
# Wires ilar does not speak to these families; see the catalog notes.
UNSPOKEN = ("claude-", "gemini-")
# Listed and left out on purpose; drop an entry to have it looked at again.
EXCLUDED = {
    ("opencode", "glm-5"): "past OpenCode's published deprecation (2026-09-03)",
    ("opencode", "kimi-k2.5"): "past OpenCode's published deprecation (2026-09-03)",
    ("opencode", "minimax-m2.5"): "past OpenCode's published deprecation (2026-09-03)",
    ("opencode", "gpt-5-codex"): "past OpenCode's published deprecation (2026-09-03)",
    ("opencode", "gpt-5.1-codex"): "past OpenCode's published deprecation (2026-09-03)",
    ("opencode", "gpt-5.1-codex-max"): "past OpenCode's published deprecation (2026-09-03)",
    ("opencode", "gpt-5.1-codex-mini"): "past OpenCode's published deprecation (2026-09-03)",
    ("opencode", "gpt-5.2-codex"): "past OpenCode's published deprecation (2026-09-03)",
    ("opencode", "gpt-5.3-codex-spark"): "listed, but the upstream has no route (2026-09-03)",
    ("opencode", "mimo-v2.6-flash-free"): "free tier only inside OpenCode (2026-09-24)",
    ("opencode-go", "minimax-m2.7"): "503 upstream while the Zen twin answers (2026-09-24)",
}


def api_key():
    key = os.environ.get("OPENCODE_GO")
    env = Path.home() / ".env"
    if not key and env.exists():
        for line in env.read_text().splitlines():
            if line.startswith("OPENCODE_GO="):
                key = line.split("=", 1)[1].strip().strip('"')
    if not key:
        sys.exit("no OPENCODE_GO in the environment or ~/.env")
    return key


def request(url, key=None, body=None):
    headers = {"Content-Type": "application/json", "User-Agent": "ilar-opencode-diff"}
    if key:
        headers |= {
            "Authorization": f"Bearer {key}",
            "x-opencode-session": str(uuid.uuid4()),
            "x-opencode-client": "ilar",
        }
    data = json.dumps(body).encode() if body is not None else None
    try:
        with urllib.request.urlopen(urllib.request.Request(url, data, headers), timeout=90) as r:
            return r.status, r.read().decode()
    except urllib.error.HTTPError as error:
        return error.code, error.read().decode()


def cataloged():
    source = CATALOG.read_text()
    rows = re.findall(r'model!\(\s*"(opencode(?:-go)?)",\s*"([^"]+)"',
                      source[source.index("static CATALOG"):])
    return {gateway: {i for p, i in rows if p == gateway} for gateway in GATEWAYS}


def wire(record):
    npm = (record.get("provider") or {}).get("npm", "")
    return "responses" if npm == "@ai-sdk/openai" else "chat"


def verdict(model_id, record):
    if record is None:
        return "not on models.dev"
    if model_id.startswith(UNSPOKEN):
        return "wire ilar does not speak"
    deprecation = record.get("deprecation_date")
    if record.get("status") == "deprecated" or (deprecation and deprecation <= str(date.today())):
        return f"deprecated {deprecation or ''}".strip()
    return None


def probe(base, key, model_id, on):
    if on == "responses":
        body = {"model": model_id, "input": "Say ok.", "max_output_tokens": 64}
    else:
        body = {"model": model_id, "messages": [{"role": "user", "content": "Say ok."}],
                "max_tokens": 64}
    status, text = request(f"{base}/{on}" if on == "responses" else f"{base}/chat/completions",
                           key, body)
    return status, "" if status == 200 else text[:140].replace("\n", " ")


def main():
    key = api_key()
    probing = "--probe" in sys.argv[1:]
    known = cataloged()
    _, text = request("https://models.dev/api.json")
    records = json.loads(text)
    for gateway, base in GATEWAYS.items():
        _, text = request(f"{base}/models", key)
        listed = {model["id"] for model in json.loads(text)["data"]}
        described = records.get(gateway, {}).get("models", {})
        print(f"== {gateway}: {len(listed)} listed, {len(known[gateway])} cataloged")
        for model_id in sorted(listed - known[gateway]):
            record = described.get(model_id)
            skip = EXCLUDED.get((gateway, model_id)) or verdict(model_id, record)
            if skip:
                print(f"  skip {model_id}: {skip}")
                continue
            fields = {k: record.get(k) for k in ("name", "release_date", "limit", "modalities",
                                                  "reasoning_options", "interleaved", "cost")}
            print(f"  NEW  {model_id} [{wire(record)}] {json.dumps(fields)}")
            if probing:
                status, why = probe(base, key, model_id, wire(record))
                print(f"       probe: {status} {why}")
        for model_id in sorted(known[gateway] - listed):
            print(f"  GONE {model_id}: cataloged, no longer listed")


if __name__ == "__main__":
    main()
