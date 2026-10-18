"""Ingest the #304 reproduction corpus into trawld.

    python3 -I gen-events.py URL CACERT AUTH_HEADER_FILE EVENTS MSG_CHARS SEED [BATCH]

Every event is {"_time", "service": "i304", "host", "seq", "message"}, where
message is MSG_CHARS characters of base64 over seeded random bytes: high
cardinality and close to incompressible. The same SEED yields the same
messages. _time spreads the events evenly over the 48 hours before the run
started (trawld partitions by arrival hour, so the corpus still lands in one
or two hourly Parquet files). BATCH events (default 4000) go to each POST
/api/v1/ingest as NDJSON; a 503 hot_buffer_full is retried after its
Retry-After, and a dropped connection after 1 s. The bearer header (a
"Name: value" line) is read from a file and never printed.
"""

import base64
import datetime as dt
import json
import random
import ssl
import sys
import time
import urllib.error
import urllib.request

url, cacert, header_file, events, msg_chars, seed = sys.argv[1:7]
events, msg_chars, seed = int(events), int(msg_chars), int(seed)
with open(header_file, encoding="utf-8") as f:
    header_name, header_value = (part.strip() for part in f.read().split(":", 1))
ctx = ssl.create_default_context(cafile=cacert)
rng = random.Random(seed)
SPAN = dt.timedelta(hours=48)
start = dt.datetime.now(dt.timezone.utc) - SPAN
step = SPAN / events
BATCH = int(sys.argv[7]) if len(sys.argv) > 7 else 4000
raw_bytes = (msg_chars * 3 + 3) // 4


def post(body):
    for attempt in range(600):
        req = urllib.request.Request(url, data=body, method="POST", headers={
            header_name: header_value, "Content-Type": "application/x-ndjson"})
        try:
            with urllib.request.urlopen(req, context=ctx, timeout=120) as resp:
                out = json.load(resp)
                if out.get("rejected"):
                    sys.exit(f"ingest rejected events: {out}")
                return out["accepted"]
        except urllib.error.HTTPError as e:
            if e.code == 503:
                time.sleep(float(e.headers.get("Retry-After") or 1))
                continue
            sys.exit(f"ingest failed: HTTP {e.code} {e.read()[:300]!r}")
        except (urllib.error.URLError, ConnectionError, TimeoutError) as e:
            # The port-forward drops a connection trawld hangs up on mid-upload
            # (a refused body); its supervisor reconnects, and the batch is
            # sent again. The final count in the transcript shows any repeat.
            print(f"  ingest connection error, retrying: {e}", flush=True)
            time.sleep(1)
            continue
    sys.exit("ingest: hot buffer stayed full")


accepted = 0
began = time.monotonic()
sent_bytes = 0
for first in range(0, events, BATCH):
    lines = []
    for seq in range(first, min(first + BATCH, events)):
        msg = base64.b64encode(rng.randbytes(raw_bytes)).decode()[:msg_chars]
        lines.append(json.dumps({
            "_time": (start + step * seq).strftime("%Y-%m-%dT%H:%M:%S.%fZ"),
            "service": "i304", "host": f"gen-{seq % 16}", "seq": seq, "message": msg,
        }, separators=(",", ":")))
    body = ("\n".join(lines) + "\n").encode()
    sent_bytes += len(body)
    accepted += post(body)
    if (first // BATCH) % 25 == 0:
        print(f"  ingested {accepted}/{events} events, {sent_bytes / 2**20:.0f} MiB, "
              f"{time.monotonic() - began:.0f}s", flush=True)
print(f"ingested {accepted} events, {sent_bytes / 2**20:.0f} MiB of NDJSON in "
      f"{time.monotonic() - began:.0f}s")
if accepted != events:
    sys.exit("accepted count differs from events sent")
