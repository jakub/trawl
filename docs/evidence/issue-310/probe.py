#!/usr/bin/env python3
"""Drive `trawl mcp` over stdio with the transcripts' tool calls, no model in
the loop, and record every request and response: first the agent's session on
the `agent` profile, then the person's on the read-only `human` profile.
Usage: probe.py <trawl-binary> <private-home> <out.jsonl>"""
import json
import re
import subprocess
import sys

trawl, home, out_path = sys.argv[1:]
out = open(out_path, "w")


class Session:
    def __init__(self, profile):
        self.profile = profile
        self.next_id = 0
        self.proc = subprocess.Popen([trawl, "mcp", "--profile", profile], stdin=subprocess.PIPE,
                                     stdout=subprocess.PIPE, env={"HOME": home, "PATH": "/usr/bin:/bin"}, text=True)
        self.send("initialize", {"protocolVersion": "2025-11-25", "capabilities": {},
                                 "clientInfo": {"name": "trawl-310-probe", "version": "1"}})
        self.send("notifications/initialized", notify=True)
        self.send("tools/list")

    def send(self, method, params=None, notify=False):
        frame = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            frame["params"] = params
        if not notify:
            frame["id"] = self.next_id
            self.next_id += 1
        self.proc.stdin.write(json.dumps(frame) + "\n")
        self.proc.stdin.flush()
        out.write(json.dumps({"profile": self.profile, "request": frame}) + "\n")
        if notify:
            return None
        while True:
            reply = json.loads(self.proc.stdout.readline())
            if reply.get("id") == frame["id"]:
                out.write(json.dumps({"profile": self.profile, "response": reply}) + "\n")
                return reply

    def call(self, name, arguments):
        result = self.send("tools/call", {"name": name, "arguments": arguments})["result"]
        text = result["content"][0]["text"]
        print(f"--- [{self.profile}] {name} {json.dumps(arguments)} isError={result.get('isError', False)}\n{text}\n")
        return text

    def close(self):
        self.proc.stdin.close()
        print(f"trawl mcp --profile {self.profile} exit", self.proc.wait())


def created_id(text):
    return re.search(r'Created investigation "([0-9a-f]{32})"', text).group(1)


def kept_id(text):
    return re.search(r'result: "([0-9a-f]{32})", kept', text).group(1)


agent = Session("agent")
agent.call("orient", {})
created = agent.call("investigation", {"action": "create", "title": "502s on checkout",
                                       "question": "Why does checkout return 502 since the last deploy?",
                                       "scope": "env=prod: nginx, shop-api, postgres"})
inv = created_id(created)
first = agent.call("search", {"mode": "execute", "query": "service=nginx status=502 last=1h", "rows": 20,
                              "investigation": inv, "purpose": "Find when checkout started answering 502"})
r1 = kept_id(first)
agent.call("investigation", {"action": "keep", "id": inv, "result_id": r1, "rows": [0, 9],
                             "reason": "The last and the first 502 of the burst"})
second = agent.call("search", {"mode": "execute", "query": "service=shop-api last=1h", "rows": 30,
                               "investigation": inv, "purpose": "See what shop-api logged around the first 502"})
r2 = kept_id(second)
agent.call("investigation", {"action": "finding", "id": inv, "status": "supported",
                             "statement": "Checkout answers 502 because shop-api exhausts its database pool "
                                          "right after deploy 4812.",
                             "references": [{"result_id": r1, "row": 9}, {"result_id": r2, "row": 9}],
                             "remaining_question": "Why does deploy 4812 hold connections longer?"})
agent.call("investigation", {"action": "read", "id": inv})
agent.close()

human = Session("human")
human.call("orient", {})
human.call("investigation", {"action": "read", "id": inv})
human.call("read", {"action": "page", "result_id": r1, "offset": 9, "rows": 1})
human.call("investigation", {"action": "note", "id": inv, "text": "Rolling back 4812."})
human.close()
