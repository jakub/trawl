#!/usr/bin/env python3
"""Render a Claude Code stream-json transcript as Markdown: the tool calls, what each returned, the final answer."""
import json, sys
src, dst = sys.argv[1], sys.argv[2]
calls, results, final = [], {}, None
for line in open(src):
    try:
        d = json.loads(line)
    except ValueError:
        continue
    if d.get("type") == "assistant":
        for b in d["message"].get("content", []):
            if b.get("type") == "tool_use":
                calls.append((b["id"], b["name"], b.get("input")))
    elif d.get("type") == "user":
        for b in d["message"].get("content", []):
            if b.get("type") == "tool_result":
                c = b.get("content")
                text = c if isinstance(c, str) else "".join(x.get("text", "") for x in c if isinstance(x, dict)) if isinstance(c, list) else json.dumps(c)
                results[b["tool_use_id"]] = (bool(b.get("is_error")), text)
    elif d.get("type") == "result":
        final = d
with open(dst, "w") as out:
    out.write(f"# {src.rsplit('/', 1)[-1]}\n\n")
    for i, (cid, name, inp) in enumerate(calls, 1):
        err, text = results.get(cid, (None, ""))
        out.write(f"## {i}. `{name}`\n\n```json\n{json.dumps(inp, indent=1)}\n```\n\n")
        out.write(f"Result ({'error' if err else 'ok'}, {len(text)} chars):\n\n```\n{text}\n```\n\n")
    if final:
        out.write(f"## Final answer ({final.get('num_turns')} turns)\n\n{final.get('result', '')}\n")
print(dst, len(calls), "calls")
