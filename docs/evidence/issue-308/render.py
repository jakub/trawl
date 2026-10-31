#!/usr/bin/env python3
"""Render a sanitize.py transcript as Markdown: the tool calls, what each returned, the final answer."""
import json, sys
src, dst = sys.argv[1], sys.argv[2]
calls, results, final, turns = [], {}, None, None
for line in open(src, encoding="utf-8"):
    d = json.loads(line)
    if d["event"] == "tool_use":
        calls.append((d["call"], d["tool"], d["input"]))
    elif d["event"] == "tool_result":
        c = d["content"]
        text = c if isinstance(c, str) else "".join(x.get("text", "") for x in c if isinstance(x, dict)) if isinstance(c, list) else json.dumps(c)
        results[d["call"]] = (d["is_error"], text)
    elif d["event"] == "text":
        final = d["text"]
    elif d["event"] == "result":
        turns = d["num_turns"]
with open(dst, "w", encoding="utf-8") as out:
    out.write(f"# {src.rsplit('/', 1)[-1]}\n\n")
    for n, name, inp in calls:
        err, text = results.get(n, (None, ""))
        out.write(f"## {n}. `{name}`\n\n```json\n{json.dumps(inp, indent=1)}\n```\n\n")
        out.write(f"Result ({'error' if err else 'ok'}, {len(text)} chars):\n\n```\n{text}\n```\n\n")
    if turns is not None:
        out.write(f"## Final answer ({turns} turns)\n\n{final or ''}\n")
print(dst, len(calls), "calls")
