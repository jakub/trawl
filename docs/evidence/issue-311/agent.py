#!/usr/bin/env python3
"""Drive the agent's half of the #311 two-principal loop over `trawl mcp`.

One `trawl mcp --profile agent` process stays open for the whole scenario,
so the `seen_seq` it holds for the investigation persists from the first
attached search to the last. No model is in the loop: the calls are fixed.
The browser half (board.mjs) runs beside it, and the two hand over through
files in <signals>:

  agent  writes investigation.json  the investigation is created and holds
                                    two steps, a finding and a note
  board  writes board-open          the board is open on a phone, scrolled
  agent  writes wrote.json          the AC1 search (call 7) has returned
  board  writes kept.json           the person kept a row from the board
  agent  writes saw-keep.json       call 8 has returned
  board  writes sent.json           the person sent two searches from Search
  agent  writes done.json           call 9 has returned and the process exited

Either side that stops early writes agent-failed.json or board-failed.json,
and the other stops waiting.

Usage: agent.py <trawl-binary> <private-home> <signals-dir> <out-dir>

Writes <out-dir>/agent-frames.jsonl, every JSON-RPC frame sent and received
in order, each with the UTC instant it was logged; and <out-dir>/agent.txt,
each tool call and the text block it returned. The process's stderr goes to
<out-dir>/agent.stderr, which is not committed.
"""
import datetime as dt
import json
import os
import re
import subprocess
import sys
import time

trawl, home, signals, out_dir = sys.argv[1:]
frames = open(os.path.join(out_dir, "agent-frames.jsonl"), "w")
text_log = open(os.path.join(out_dir, "agent.txt"), "w")
WAIT_SECS = 600


def now():
    return dt.datetime.now(dt.timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z")


def signal(name, body):
    path = os.path.join(signals, name)
    with open(path + ".tmp", "w") as f:
        json.dump({"at": now(), **body}, f)
    os.rename(path + ".tmp", path)


def wait_for(name):
    path = os.path.join(signals, name)
    deadline = time.monotonic() + WAIT_SECS
    while not os.path.exists(path):
        if os.path.exists(os.path.join(signals, "board-failed.json")):
            fail(f"the board failed while the agent waited for {name}")
        if time.monotonic() > deadline:
            fail(f"no {name} from the board within {WAIT_SECS} s")
        time.sleep(0.2)
    with open(path) as f:
        body = json.load(f)
    log_text(f"=== waited for {name}: {json.dumps(body)}\n")
    return body


def fail(message):
    """Tell the board the agent stopped, then stop."""
    signal("agent-failed.json", {"message": message})
    sys.exit(f"agent: {message}")


def log_text(text):
    text_log.write(text + "\n")
    text_log.flush()
    print(text, flush=True)


class Session:
    def __init__(self):
        self.next_id = 0
        self.calls = 0
        self.stderr = open(os.path.join(out_dir, "agent.stderr"), "w")
        self.proc = subprocess.Popen([trawl, "mcp", "--profile", "agent"], stdin=subprocess.PIPE,
                                     stdout=subprocess.PIPE, stderr=self.stderr,
                                     env={"HOME": home, "PATH": "/usr/bin:/bin"}, text=True)
        self.send("initialize", {"protocolVersion": "2025-11-25", "capabilities": {},
                                 "clientInfo": {"name": "trawl-311-agent", "version": "1"}})
        self.send("notifications/initialized", notify=True)
        self.send("tools/list")

    def log(self, key, frame):
        frames.write(json.dumps({"at": now(), key: frame}) + "\n")
        frames.flush()

    def send(self, method, params=None, notify=False):
        frame = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            frame["params"] = params
        if not notify:
            frame["id"] = self.next_id
            self.next_id += 1
        self.proc.stdin.write(json.dumps(frame) + "\n")
        self.proc.stdin.flush()
        self.log("request", frame)
        if notify:
            return None
        while True:
            line = self.proc.stdout.readline()
            if not line:
                fail(f"trawl mcp closed its output before answering {method}")
            reply = json.loads(line)
            self.log("response", reply)
            if reply.get("id") == frame["id"]:
                return reply

    def call(self, name, arguments):
        self.calls += 1
        result = self.send("tools/call", {"name": name, "arguments": arguments})["result"]
        text = result["content"][0]["text"]
        log_text(f"--- call {self.calls}: {name} {json.dumps(arguments)} isError={result.get('isError', False)}\n{text}\n")
        if result.get("isError"):
            fail(f"call {self.calls} ({name}) was refused")
        return result

    def close(self):
        self.proc.stdin.close()
        code = self.proc.wait()
        log_text(f"trawl mcp --profile agent exit {code}")
        return code


def kept_id(result):
    return result["structuredContent"]["result"]["result_id"]


agent = Session()
agent.call("orient", {})
created = agent.call("investigation", {"action": "create", "title": "502s on checkout",
                                       "question": "Why does checkout return 502 since the last deploy?",
                                       "scope": "env=prod: nginx, shop-api, postgres"})
inv = created["structuredContent"]["investigation"]["id"]
first = agent.call("search", {"mode": "execute", "query": "service=nginx status=502 last=1h", "rows": 10,
                              "investigation": inv, "purpose": "Find when checkout started answering 502"})
r1 = kept_id(first)
second = agent.call("search", {"mode": "execute", "query": "service=shop-api last=1h", "rows": 12,
                               "investigation": inv, "purpose": "See what shop-api logged around the first 502"})
r2 = kept_id(second)
agent.call("investigation", {"action": "finding", "id": inv, "status": "supported",
                             "statement": "Checkout answers 502 because shop-api exhausts its database pool "
                                          "right after deploy 4812.",
                             "references": [{"result_id": r1, "row": 9}, {"result_id": r2, "row": 9}],
                             "remaining_question": "Why does deploy 4812 hold connections longer?"})
agent.call("investigation", {"action": "note", "id": inv,
                             "text": "Next: what postgres logged while the pool ran dry."})
signal("investigation.json", {"id": inv, "first_result": r1, "second_result": r2})

# AC1: the agent writes while the person reads the board on a phone.
wait_for("board-open")
agent.call("search", {"mode": "execute", "query": "service=postgres last=1h", "rows": 6,
                      "investigation": inv, "purpose": "See whether postgres refused connections"})
signal("wrote.json", {"call": agent.calls})

# AC2: the person's keep arrives in the next attached result, once.
wait_for("kept.json")
agent.call("search", {"mode": "execute", "query": "service=nginx path=/healthz last=1h", "rows": 6,
                      "investigation": inv, "purpose": "Check whether health checks kept passing"})
signal("saw-keep.json", {"call": agent.calls})

# AC3's server half: the person's two sends arrive; the keep does not again.
wait_for("sent.json")
agent.call("search", {"mode": "execute", "query": "service=shop-api level=error last=1h", "rows": 6,
                      "investigation": inv, "purpose": "Count the pool exhaustion errors"})
code = agent.close()
signal("done.json", {"call": agent.calls, "exit": code})
sys.exit(code)
