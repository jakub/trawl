#!/usr/bin/env python3
"""Reduce each client frame's `params._meta` to its MCP-standard keys.

Usage: python3 -I frames.py <frames jsonl> <out jsonl>

Reads the client frames `mcp-tee.sh` logged, one JSON-RPC message per line,
and writes every frame as it is, except that `params._meta` keeps only the
keys under `io.modelcontextprotocol/`. That drops what Claude Code adds to
each request, its tool-use id under the `claudecode/` prefix and the
`progressToken`, and keeps the protocol version, client info and client
capabilities. A `_meta` left
empty is removed. Frames are written in the same compact form `mcp-tee.sh`
logged them, so a frames file run through this again does not change, and
<out> may be <frames jsonl>. The output is then scanned with check.py's
rules, and any hit fails the run.
"""
import json
import os
import runpy
import sys

STANDARD = "io.modelcontextprotocol/"


def reduce(frame):
    params = frame.get("params")
    if isinstance(params, dict) and isinstance(params.get("_meta"), dict):
        meta = {k: v for k, v in params["_meta"].items() if k.startswith(STANDARD)}
        if meta:
            params["_meta"] = meta
        else:
            del params["_meta"]
    return frame


def main(src, dst):
    with open(src, encoding="utf-8") as f:
        frames = [reduce(json.loads(line)) for line in f if line.strip()]
    with open(dst, "w", encoding="utf-8") as f:
        for frame in frames:
            f.write(json.dumps(frame, ensure_ascii=False, separators=(",", ":")) + "\n")
    check = runpy.run_path(os.path.join(os.path.dirname(os.path.abspath(__file__)), "check.py"))
    found = list(check["hits"](dst))
    for n, pattern in found:
        print(f"{dst}:{n}: {pattern}")
    print(dst, f"{len(frames)} frames", f"{len(found)} check hits")
    return 1 if found else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1], sys.argv[2]))
