#!/usr/bin/env python3
"""Verify the #311 recording: a privacy scan, then the two-principal claims.

Usage: python3 -I check.py <dir>

The privacy scan reads every file under <dir> except the PNG captures and
fails, naming file, line and pattern, on issue-310's rules: an absolute
home path, a cost, usage, signature, session id or uuid field, the bearer
scheme followed by a token, a `trawl_` or `flt_` key, Claude Code's
tool-use id key, or a `toolu_` id; and on a cookie header, a storage
state's cookie list or trawl-web's session cookie name, where issue-310
refused the word itself (stack.sh here writes trawl-web's cookie config
keys). Each pattern is spelled so that this file does not match itself.

The claims are read from agent-frames.jsonl (every JSON-RPC frame agent.py
logged), board.jsonl (every observation board.mjs logged), keys.txt and
the two aria files:

- AC1: the agent's step reached the open phone board within 10 s of the
  board saying it was open, and the element that holds the reader's place
  is the finding they read and moved at most 1 px.
- AC2: the person's keep is in the activity of the agent's first attached
  call after it, exactly once, under the writer key's prefix and name, with
  its row and reason; no later attached call prints it again.
- AC3: each send's capture request carried the investigation and a window;
  the server's step, as the agent's last attached call printed it, is the
  writer's, with bounds equal to that window and the executed text the
  capture answered and the board showed. The viewer paged the same events
  as the Search table, by `_raw`, and every displayed cell of every row, the
  `_severity` token included, equals Search's as a set of rows (order is
  noted, not asserted). The moving range was pinned to the
  whole-second hull of [started_at - 1h, started_at] of the search that
  ran, and sent as [floor, ceil + 1 µs).
- AC4: the read-only person's board header, viewer row and Search actions
  hold no Close, Reopen, Keep or Send control; the writer's hold them.

Prints one line per claim and exits 1 on any failure.
"""
import datetime as dt
import json
import os
import re
import sys

RULES = [
    re.compile(r"(?<![\w}])/hom[e]/"),
    re.compile(r"total_cost_us[d]"),
    re.compile(r'"usag[e]"'),
    re.compile(r'"signatur[e]"'),
    re.compile(r"session_i[d]"),
    re.compile(r'"uui[d]"'),
    re.compile(r"Beare[r] [A-Za-z0-9._~+/-]"),
    re.compile(r"trawl_[A-Za-z0-9]"),
    re.compile(r"flt_[A-Za-z0-9_-]"),
    # A cookie header, a storage state's cookie list, or the session
    # cookie by name; not the trawl-web config keys stack.sh writes
    # (`allow_insecure_` + `cookies`, `cookie` + `_secret_env`).
    re.compile(r"(?<![_\w])[Cc]ooki[e]s?[\"']?\s*[:=]"),
    re.compile(r"fleet_sessio[n]"),
    re.compile(r"claudecode/toolUseI[d]"),
    re.compile(r"toolu[_][A-Za-z0-9]"),
]

failures = []


def claim(ok, text):
    print(("ok   " if ok else "FAIL ") + text)
    if not ok:
        failures.append(text)


def privacy(root):
    files = sorted(os.path.join(d, n) for d, _, names in os.walk(root) for n in names if not n.endswith(".png"))
    found = 0
    for path in files:
        with open(path, "rb") as f:
            for n, raw in enumerate(f, 1):
                line = raw.decode("utf-8", errors="replace")
                for rule in RULES:
                    if rule.search(line):
                        print(f"{os.path.relpath(path, root)}:{n}: {rule.pattern}")
                        found += 1
    claim(found == 0, f"privacy: {len(files)} files, {found} hits")


def instant(text):
    return dt.datetime.fromisoformat(text.replace("Z", "+00:00"))


def wire(at):
    return at.astimezone(dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%fZ")


def agent_calls(root):
    """Each tools/call in order: (name, arguments, structuredContent,
    isError, the instant its request was logged)."""
    requests, calls = {}, []
    for line in open(os.path.join(root, "agent-frames.jsonl"), encoding="utf-8"):
        frame = json.loads(line)
        if "request" in frame and frame["request"].get("method") == "tools/call":
            requests[frame["request"]["id"]] = (frame["request"]["params"], instant(frame["at"]))
        elif "response" in frame and frame["response"].get("id") in requests:
            params, at = requests.pop(frame["response"]["id"])
            result = frame["response"]["result"]
            calls.append((params["name"], params["arguments"], result.get("structuredContent") or {},
                          result.get("isError", False), at))
    return calls


def writer_prefix(root):
    for line in open(os.path.join(root, "keys.txt"), encoding="utf-8"):
        cells = [c.strip() for c in re.split(r"[│┆]", line) if c.strip()]
        if len(cells) >= 2 and cells[1] == "evidence-writer":
            return cells[0]
    return None


def main(root):
    privacy(root)
    board = [json.loads(line) for line in open(os.path.join(root, "board.jsonl"), encoding="utf-8")]
    events = lambda name: [e for e in board if e["event"] == name]
    claim(not events("failed") and not events("pageerror"), "board.mjs: no failure and no page error")
    calls = agent_calls(root)
    claim(len(calls) == 9 and not any(c[3] for c in calls), f"agent.py: {len(calls)} tool calls, none refused")
    attached = [(i + 1, c[2]["investigation"]["activity"], c[4]) for i, c in enumerate(calls)
                if c[0] == "search" and "investigation" in c[1]]
    inv = calls[1][2]["investigation"]["id"]
    prefix = writer_prefix(root)

    # AC1
    ac1 = events("ac1 pass")
    before, after = events("ac1 before"), events("ac1 after")
    claim(len(ac1) == 1 and ac1[0]["arrived_ms"] <= 10_000,
          f"AC1: the agent's step reached the board {ac1[0]['arrived_ms'] if ac1 else '?'} ms after it opened (at most 10000)")
    claim(len(before) == 1 and len(after) == 1 and before[0]["anchor"]["key"] == before[0]["reading"] == after[0]["anchor"]["key"]
          and abs(after[0]["anchor"]["offset"] - before[0]["anchor"]["offset"]) <= 1 and abs(after[0]["top"] - before[0]["top"]) <= 1,
          f"AC1: {before[0]['reading'] if before else '?'} holds the reader's place and moved "
          f"{after[0]['top'] - before[0]['top'] if after else '?'} px while the board grew by "
          f"{after[0]['scrollHeight'] - before[0]['scrollHeight'] if after else '?'} px")

    # AC2
    keep = [e for e in events("entry write") if e["request"]["kind"] == "keep"]
    claim(len(keep) == 1 and keep[0]["status"] == 201, "AC2: the board wrote one keep, answered 201")
    entry = keep[0]["response"]["entry"]
    seen = [(n, [e for e in a["entries"] if e["seq"] == entry["seq"]]) for n, a, _ in attached]
    printed = [(n, e) for n, es in seen for e in es]
    first_after = next(n for n, _, at in attached if at > instant(entry["at"]))
    claim(len(printed) == 1 and printed[0][0] == first_after,
          f"AC2: the keep (#{entry['seq']}) printed once, in call {printed[0][0] if printed else '?'}, the first attached call after it")
    if printed:
        e = printed[0][1]
        claim(e["actor"] == {"prefix": prefix, "name": "evidence-writer"} and e["payload"] == keep[0]["request"],
              f"AC2: it names {e['actor']['prefix']} \"{e['actor']['name']}\", rows {e['payload']['rows']} of "
              f"{e['payload']['result_id'][:8]} and the reason the board sent")
    later = [n for n, _, _ in attached if n > first_after]
    claim(later and all(entry["seq"] not in [x["seq"] for x in a["entries"]] for n, a, _ in attached if n in later),
          f"AC2: no later attached call ({', '.join(map(str, later))}) prints the keep again")

    # AC3
    captures = events("capture")
    steps = events("ac3 board step")
    last = attached[-1][1]["entries"]
    claim(len(captures) == 2 and all(c["status"] == 200 and c["request"]["investigation"]["id"] == inv for c in captures),
          "AC3: two captures sent to the agent's investigation, both answered 200")
    for capture, step in zip(captures, steps):
        window = capture["request"]["window"]
        executed = capture["response"]["result"]["executed_query"]
        server = [e for e in last if e["payload"].get("purpose") == capture["request"]["investigation"].get("purpose")]
        ok = (len(server) == 1 and server[0]["actor"]["prefix"] == prefix
              and server[0]["payload"]["bounds"] == window
              and server[0]["payload"]["executed"] == executed.replace("\n", "\\n")
              and step["executed"] == executed)
        claim(ok, f"AC3: \"{step['purpose']}\" is the writer's step with bounds [{window['earliest']}, {window['latest']}) and the executed text the board shows")
        cols = step["columns"]
        raw = cols.index("_raw")
        same = sorted(r[raw] for r in step["viewer_rows"]) == sorted(r[raw] for r in step["search_rows"])
        claim(same and len(step["viewer_rows"]) == len(step["search_rows"]),
              f"AC3: its viewer paged the {len(step['viewer_rows'])} events the Search table showed")
        sev = cols.index("_severity")
        rows = lambda key: sorted(json.dumps(r) for r in step[key])
        claim(rows("viewer_rows") == rows("search_rows") and step["same_cells"],
              f"AC3: every cell of those rows is the one Search showed, as a set; `_severity` reads "
              f"{sorted({r[sev] for r in step['viewer_rows']})} in the viewer and "
              f"{sorted({r[sev] for r in step['search_rows']})} in Search")
        print(f"note order: {'the same' if step['same_cells_in_order'] else 'the viewer newest first, Search as the query ran'}")
    queries = events("search query")
    quick = [q for q in queries if q["query"].startswith("last=1h ")]
    if len(captures) == 2 and quick:
        # started_at carries nanoseconds, more than datetime holds: split
        # it into its whole second and the fraction past it.
        started = quick[0]["execution"]["started_at"].rstrip("Z")
        whole = instant(started[:19] + "Z")
        past = started[20:].strip("0") != ""
        floor_from = whole - dt.timedelta(hours=1)
        ceil_to = whole + dt.timedelta(seconds=1 if past else 0)
        window = captures[1]["request"]["window"]
        claim(window == {"earliest": wire(floor_from), "latest": wire(ceil_to + dt.timedelta(microseconds=1))},
              f"AC3: the moving range ran at {quick[0]['execution']['started_at']} and was sent pinned as "
              f"[{window['earliest']}, {window['latest']}), whole seconds plus the 1 µs of an inclusive end")
        rerun = f'_time>="{floor_from:%Y-%m-%dT%H:%M:%SZ}" _time<="{ceil_to:%Y-%m-%dT%H:%M:%SZ}" service=shop-api'
        claim(any(q["query"] == rerun for q in queries), f"AC3: Search ran the pinned range again before it sent: {rerun}")

    # AC4
    ac4 = events("ac4")
    claim(len(ac4) == 1 and ac4[0]["reader"] == {"close": False, "reopen": False, "keep": False, "send": False}
          and ac4[0]["writer"]["close"] and ac4[0]["writer"]["keep"] and ac4[0]["writer"]["send"],
          "AC4: the reader has no Close, Reopen, Keep or Send; the writer has Close, Keep and Send")
    reader = open(os.path.join(root, "reader-aria.txt"), encoding="utf-8").read()
    claim(not re.search(r'button "(Close|Reopen) the investigation"|button "Keep row|button "Send to investigation"', reader),
          "AC4: reader-aria.txt names none of those buttons")

    print(f"check: {len(failures)} failures")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1]))
