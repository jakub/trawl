#!/usr/bin/env python3
"""Write the #310 seed: one incident for the agent to investigate.

Checkout starts answering 502 on nginx a minute after deploy 4812 of shop-api,
because shop-api exhausts its database pool. The events count back from the
second this runs, all in the last hour, all env=prod.
Usage: seed.py <seed.ndjson>"""
import datetime as dt
import json
import sys

out_path = sys.argv[1]
now = dt.datetime.now(dt.timezone.utc).replace(microsecond=0)
deploy = now - dt.timedelta(minutes=32)
events = []


def event(at, service, host, level, message, **fields):
    events.append({"_time": at.isoformat().replace("+00:00", "Z"), "env": "prod", "service": service,
                   "host": host, "level": level, "message": message, **fields})


# Before the deploy: checkout works.
for i in range(8):
    at = deploy - dt.timedelta(minutes=20) + dt.timedelta(minutes=2 * i)
    event(at, "nginx", "web-02", "info", f"POST /checkout 200 order={7700 + i}", status=200, path="/checkout")
    event(at + dt.timedelta(seconds=1), "shop-api", "app-01", "info", f"order {7700 + i} placed", status=200)
# The deploy.
event(deploy, "shop-api", "app-01", "info", "deploy 4812 started", deploy="4812")
event(deploy + dt.timedelta(seconds=40), "shop-api", "app-01", "info", "deploy 4812 finished; pool size 20",
      deploy="4812")
# After it: the pool runs dry, postgres refuses, nginx answers 502.
for i in range(10):
    at = deploy + dt.timedelta(minutes=1, seconds=30 * i)
    event(at, "shop-api", "app-01", "error", f"db pool exhausted: connection refused (pool=20, waiting={41 + 2 * i})",
          status=503)
    event(at + dt.timedelta(milliseconds=40), "nginx", "web-02", "error",
          f"POST /checkout 502 upstream prematurely closed connection order={7731 + i}", status=502, path="/checkout")
for i in range(6):
    at = deploy + dt.timedelta(minutes=1, seconds=10 + 50 * i)
    event(at, "postgres", "db-01", "warn", "FATAL: too many connections for role \"shop\"")
# Unrelated traffic that keeps working.
for i in range(6):
    at = deploy + dt.timedelta(minutes=3 * i)
    event(at, "nginx", "web-02", "info", "GET /healthz 200", status=200, path="/healthz")

with open(out_path, "w") as out:
    for e in events:
        out.write(json.dumps(e) + "\n")
print(len(events), "events written, deploy at", deploy.isoformat().replace("+00:00", "Z"))
