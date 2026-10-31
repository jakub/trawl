#!/usr/bin/env python3
"""Keep only the tool calls, tool results, model text and run summary of a Claude Code stream.

Usage: python3 -I sanitize.py <raw stream-json> <out jsonl>
       python3 -I sanitize.py --verify [file ...]

Reads a `claude -p --output-format stream-json` stream and writes one JSON
object per line, built from an allowlist:

  {"event": "tool_use", "call": N, "tool": ..., "input": ...}
  {"event": "tool_result", "call": N, "is_error": ..., "content": ...}
  {"event": "text", "text": ...}
  {"event": "init", "claude_code_version": ..., "model": ...}
  {"event": "result", "subtype": ..., "is_error": ..., "num_turns": ..., "duration_ms": ...}

`init` comes from the `system/init` event and `result` from the closing
`result` event; the final answer is the last `text` event, so `result` keeps
no text. N counts the assistant's tool_use blocks from 1 in stream order and
replaces their ids. Every other event, block and field is dropped. The output is then
checked as --verify checks it, and any failure fails the run.

--verify checks committed files, by default every tracked `*.jsonl` and
`*.ndjson` under docs/ and visual-evidence/. Each must pass check.py's rules,
which it applies to each line and to every string decoded from it, so a
raw stream fails under any name. A file named `transcript-*.jsonl`, or one
holding a line that pairs an event name above with a key only those events
use (`call`, `tool`, `claude_code_version`, `num_turns`, `duration_ms`),
must hold only those events, each with exactly its keys. It exits 1 on any
failure.
"""
import json
import os
import runpy
import subprocess
import sys

# The keys of each event the sanitized transcript may hold, in written order.
SHAPES = {
    "tool_use": ("event", "call", "tool", "input"),
    "tool_result": ("event", "call", "is_error", "content"),
    "text": ("event", "text"),
    "init": ("event", "claude_code_version", "model"),
    "result": ("event", "subtype", "is_error", "num_turns", "duration_ms"),
}


def sanitize(src):
    calls = {}
    out = []
    for line in open(src, encoding="utf-8"):
        try:
            d = json.loads(line)
        except ValueError:
            continue
        kind = d.get("type")
        if kind == "system" and d.get("subtype") == "init":
            out.append({"event": "init", "claude_code_version": d.get("claude_code_version"), "model": d.get("model")})
            continue
        if kind == "result":
            out.append({"event": "result", **{k: d.get(k) for k in SHAPES["result"][1:]}})
            continue
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


def strings(value):
    """Yield every key and string inside a decoded JSON value."""
    if isinstance(value, str):
        yield value
    elif isinstance(value, dict):
        for k, v in value.items():
            yield k
            yield from strings(v)
    elif isinstance(value, list):
        for v in value:
            yield from strings(v)


# Keys no log row is expected to pair with a sanitize.py event name.
TRANSCRIPT_ONLY = {"call", "tool", "claude_code_version", "num_turns", "duration_ms"}


def is_transcript_event(d):
    return isinstance(d, dict) and isinstance(d.get("event"), str) and d["event"] in SHAPES and bool(TRANSCRIPT_ONLY & set(d))


def verify(paths):
    check = runpy.run_path(os.path.join(os.path.dirname(os.path.abspath(__file__)), "check.py"))
    failed = 0

    def fail(path, n, why):
        nonlocal failed
        print(f"{path}:{n}: {why}")
        failed += 1

    for path in paths:
        for n, pattern in check["hits"](path):
            fail(path, n, pattern)
        lines = []
        for n, line in enumerate(open(path, encoding="utf-8"), 1):
            try:
                d = json.loads(line)
            except ValueError:
                d = None
            lines.append((n, d))
            # A JSON document kept as a string (a tool result, a copied
            # event) escapes its quotes, which the line rules would miss.
            for text in strings(d):
                for rule in check["RULES"]:
                    if rule.search(text) and not rule.search(line):
                        fail(path, n, f"{rule.pattern} inside a string")
        # A sanitized transcript is named transcript-*.jsonl or holds a line
        # that only sanitize.py writes; either way every line must be one of
        # its events, exactly. Log rows (seed files) may have an `event` field
        # of their own, so that alone does not mark a transcript.
        if not (os.path.basename(path).startswith("transcript-") or any(is_transcript_event(d) for _, d in lines)):
            continue
        for n, d in lines:
            keys = set(d) if isinstance(d, dict) else None
            event = d.get("event") if keys is not None else None
            if keys is None or not isinstance(event, str) or keys != set(SHAPES.get(event, ())):
                fail(path, n, f"not a sanitized event: keys {sorted(keys or ())}")
    print(f"verify: {len(paths)} files, {failed} failures")
    return 1 if failed else 0


def tracked():
    root = subprocess.run(["git", "rev-parse", "--show-toplevel"], capture_output=True, text=True, check=True).stdout.strip()
    patterns = [f":(glob){d}/**/*.{x}" for d in ("docs", "visual-evidence") for x in ("jsonl", "ndjson")]
    names = subprocess.run(["git", "-C", root, "ls-files", "-z", "--", *patterns], capture_output=True, text=True, check=True).stdout.split("\0")
    return [os.path.join(root, n) for n in names if n]


def main(src, dst):
    events = sanitize(src)
    with open(dst, "w", encoding="utf-8") as f:
        for e in events:
            f.write(json.dumps(e, ensure_ascii=False) + "\n")
    counts = {k: sum(e["event"] == k for e in events) for k in SHAPES}
    print(dst, ", ".join(f"{v} {k}" for k, v in counts.items()))
    return verify([dst])


if __name__ == "__main__":
    if sys.argv[1:2] == ["--verify"]:
        sys.exit(verify(sys.argv[2:] or tracked()))
    sys.exit(main(sys.argv[1], sys.argv[2]))
