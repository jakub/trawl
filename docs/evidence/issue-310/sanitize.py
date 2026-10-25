#!/usr/bin/env python3
"""Keep only the tool calls, tool results and model text of a Claude Code stream.

Usage: python3 -I sanitize.py <raw stream-json> <out jsonl>

Reads a `claude -p --output-format stream-json` stream and writes one JSON
object per line, built from an allowlist:

  {"event": "tool_use", "call": N, "tool": ..., "input": ...}
  {"event": "tool_result", "call": N, "is_error": ..., "content": ...}
  {"event": "text", "text": ...}

N counts the assistant's tool_use blocks from 1 in stream order and replaces
their ids. Every other event, block and field is dropped. The output is then
scanned with check.py's rules, and any hit fails the run.
"""
import json
import os
import runpy
import sys


def sanitize(src):
    calls = {}
    out = []
    for line in open(src, encoding="utf-8"):
        try:
            d = json.loads(line)
        except ValueError:
            continue
        kind = d.get("type")
        if kind not in ("assistant", "user"):
            continue
        content = d.get("message", {}).get("content")
        if not isinstance(content, list):
            continue
        for b in content:
            block = b.get("type")
            if kind == "assistant" and block == "tool_use":
                calls[b["id"]] = len(calls) + 1
                out.append({"event": "tool_use", "call": calls[b["id"]], "tool": b["name"], "input": b.get("input")})
            elif kind == "assistant" and block == "text":
                out.append({"event": "text", "text": b["text"]})
            elif kind == "user" and block == "tool_result":
                call = calls.get(b.get("tool_use_id"))
                if call is None:
                    sys.exit(f"{src}: a tool_result answers no earlier tool_use")
                out.append({"event": "tool_result", "call": call, "is_error": bool(b.get("is_error")), "content": b.get("content")})
    return out


def main(src, dst):
    events = sanitize(src)
    with open(dst, "w", encoding="utf-8") as f:
        for e in events:
            f.write(json.dumps(e, ensure_ascii=False) + "\n")
    check = runpy.run_path(os.path.join(os.path.dirname(os.path.abspath(__file__)), "check.py"))
    found = list(check["hits"](dst))
    for n, pattern in found:
        print(f"{dst}:{n}: {pattern}")
    counts = {k: sum(e["event"] == k for e in events) for k in ("tool_use", "tool_result", "text")}
    print(dst, ", ".join(f"{v} {k}" for k, v in counts.items()), f"{len(found)} check hits")
    return 1 if found else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1], sys.argv[2]))
