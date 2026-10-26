#!/usr/bin/env python3
"""Drive `trawl mcp` over stdio with the transcript's tool calls, no model in
the loop, and record every request and response. It checks the seed against
the tools: the answers a Claude Code session should relay. A refused tool
call, a JSON-RPC error or a nonzero exit of `trawl mcp` stops it with a
nonzero exit, so a failed run never passes as a recording.
Usage: probe.py <trawl-binary> <private-home> <focus.json> <out.jsonl>"""
import datetime as dt
import json
import os
import re
import subprocess
import sys

trawl, home, focus_path, out_path = sys.argv[1:]
focus = json.load(open(focus_path))
proc = subprocess.Popen([trawl, "mcp", "--profile", "agent"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                        env={"HOME": home, "PATH": "/usr/bin:/bin"}, text=True)
out = open(out_path, "w")
next_id = 0


def fail(message):
    print(f"probe failed: {message}", file=sys.stderr)
    out.close()
    proc.kill()
    proc.wait()
    sys.exit(1)


def send(method, params=None, notify=False):
    global next_id
    frame = {"jsonrpc": "2.0", "method": method}
    if params is not None:
        frame["params"] = params
    if not notify:
        frame["id"] = next_id
        next_id += 1
    proc.stdin.write(json.dumps(frame) + "\n")
    proc.stdin.flush()
    out.write(json.dumps({"request": frame}) + "\n")
    if notify:
        return None
    while True:
        line = proc.stdout.readline()
        if not line:
            fail(f"trawl mcp closed its stdout before answering {method}")
        reply = json.loads(line)
        if reply.get("id") == frame["id"]:
            out.write(json.dumps({"response": reply}) + "\n")
            if "error" in reply:
                fail(f"{method} answered a JSON-RPC error: {json.dumps(reply['error'])}")
            return reply


def call(name, arguments):
    reply = send("tools/call", {"name": name, "arguments": arguments})
    result = reply["result"]
    text = result["content"][0]["text"]
    print(f"--- {name} {json.dumps(arguments)} isError={result.get('isError', False)}\n{text}\n")
    if result.get("isError", False):
        fail(f"{name} was refused")
    return text


def result_id(text):
    return re.search(r"\b([0-9a-f]{32})\b", text).group(1)


send("initialize", {"protocolVersion": "2025-11-25", "capabilities": {},
                    "clientInfo": {"name": "trawl-309-probe", "version": "1"}})
send("notifications/initialized", notify=True)
send("tools/list")
call("orient", {})
now = dt.datetime.now(dt.timezone.utc).replace(microsecond=0)
iso = lambda t: t.isoformat().replace("+00:00", "Z")
s1 = call("sightings", {"literal": "203.0.113.77", "last": "24h"})
call("sightings", {"literal": "203.0.113.77", "last": "24h", "fields": ["client_ip"]})
found = call("search", {"mode": "execute",
                        "query": f'service={focus["service"]} host={focus["host"]} "order=7731" last=2h'})
call("context", {"result_id": result_id(found), "row": 0})
call("compare", {"filter": "level=error", "foreground_earliest": iso(now - dt.timedelta(hours=1)),
                 "foreground_latest": iso(now), "group_by": ["service"]})
call("read", {"action": "page", "result_id": result_id(s1), "offset": 0, "rows": 20})
proc.stdin.close()
status = proc.wait()
print("trawl mcp exit", status)
if status != 0:
    sys.exit(1)
