#!/usr/bin/env python3
"""Write the #309 evidence seed: three scenarios with known counts, one per
correlation read. Offsets count back from the second the seed runs; the
expected answers are in the README. Every event carries its own `_raw` line,
distinct from its fields. Usage: seed.py <seed.ndjson> <focus.json>"""
import datetime as dt
import json
import sys

now = dt.datetime.now(dt.timezone.utc).replace(microsecond=0)
out = open(sys.argv[1], "w")


def ts(t):
    return t.isoformat().replace("+00:00", "Z")


def ago(**kw):
    return now - dt.timedelta(**kw)


def ev(t, **fields):
    out.write(json.dumps({"_time": ts(t), "env": "prod", **fields}) + "\n")


IP, OTHER = "203.0.113.77", "198.51.100.9"

# 1. sightings, level=info, 110-150 min back: the IP as client_ip in nginx on
# web-02 (4 rows), as src in sshd on web-01 (3 rows), in one sshd _raw line
# that no field holds (1 row), and one nginx row with a different IP that
# must not match.
for m, path, status in [(150, "/login", 200), (140, "/login", 401), (130, "/api/cart", 200), (120, "/login", 200)]:
    t = ago(minutes=m)
    ev(t, service="nginx", host="web-02", level="info", client_ip=IP, method="GET", path=path, status=status,
       message=f"GET {path} {status}",
       _raw=f'{IP} - - [{t:%d/%b/%Y:%H:%M:%S +0000}] "GET {path} HTTP/1.1" {status} 512 "-" "curl/8.9"')
t = ago(minutes=125)
ev(t, service="nginx", host="web-02", level="info", client_ip=OTHER, method="GET", path="/", status=200,
   message="GET / 200", _raw=f'{OTHER} - - [{t:%d/%b/%Y:%H:%M:%S +0000}] "GET / HTTP/1.1" 200 64 "-" "curl/8.9"')
for i, (m, user) in enumerate([(145, "root"), (135, "admin"), (115, "deploy")]):
    t = ago(minutes=m)
    ev(t, service="sshd", host="web-01", level="info", src=IP, user=user, port=52100 + i,
       message=f"Invalid user {user}",
       _raw=f"{t:%b %d %H:%M:%S} web-01 sshd[{4100 + i}]: Invalid user {user} from {IP} port {52100 + i}")
t = ago(minutes=110)
ev(t, service="sshd", host="web-01", level="info", user="root",
   message="Connection closed by authenticating user root [preauth]",
   _raw=f"{t:%b %d %H:%M:%S} web-01 sshd[4199]: Connection closed by authenticating user root {IP} port 52199 [preauth]")

# 2. context, level=info, nginx on web-02 forty minutes back: two rows one
# second before the focus, the focus and two more at its exact _time, two
# rows one second after. Nothing else on web-02 lies within 30 s of it.
focus = ago(minutes=40)
groups = [(-1, ["GET /cart 200", "GET /cart/items 200"]),
          (0, ["POST /checkout 502 order=7731", "GET /static/app.js 200", "GET /favicon.ico 404"]),
          (1, ["GET /checkout/retry 200", "POST /checkout 201 order=7732"])]
for off, msgs in groups:
    t = focus + dt.timedelta(seconds=off)
    for j, m in enumerate(msgs):
        method, path, status = m.split()[:3]
        peer = f"10.20.0.{11 + j}"
        ev(t, service="nginx", host="web-02", level="info", client_ip=peer, method=method, path=path,
           status=int(status), message=m,
           _raw=f'{peer} - - [{t:%d/%b/%Y:%H:%M:%S +0000}] "{method} {path} HTTP/1.1" {status} 0')
json.dump({"time": ts(focus), "host": "web-02", "service": "nginx", "env": "prod", "message": groups[1][1][0]},
          open(sys.argv[2], "w"))

# 3. compare, level=error: sshd auth failures on web-01, 3 in the baseline
# hour and 30 in the foreground hour; nginx 500s on web-03, 10 and 10; cron
# job failures on web-01, 4 in the baseline hour and none after. The baseline
# rows land 75-105 min back and the foreground rows 2-45 min back, so the
# hour before the session's `now` and the hour before that hold them if the
# session starts within 15 minutes of the seed.


def spread(n, lo, hi):
    return [lo + (hi - lo) * k / max(n - 1, 1) for k in range(n)]


for n_sshd, n_nginx, n_cron, lo, hi in [(3, 10, 4, 75, 105), (30, 10, 0, 2, 45)]:
    for k, m in enumerate(spread(n_sshd, lo, hi)):
        t = ago(minutes=m)
        src = f"192.0.2.{10 + k}"
        ev(t, service="sshd", host="web-01", level="error", event="auth_failure", src=src, user="root",
           message="Failed password for root",
           _raw=f"{t:%b %d %H:%M:%S} web-01 sshd[5{k:03}]: Failed password for root from {src} port 40{k:03} ssh2")
    for k, m in enumerate(spread(n_nginx, lo, hi)):
        t = ago(minutes=m)
        ev(t, service="nginx", host="web-03", level="error", status=500, path="/api/orders",
           message="GET /api/orders 500",
           _raw=f'10.30.0.{k} - - [{t:%d/%b/%Y:%H:%M:%S +0000}] "GET /api/orders HTTP/1.1" 500 0')
    for k, m in enumerate(spread(n_cron, lo, hi)):
        t = ago(minutes=m)
        ev(t, service="cron", host="web-01", level="error", job="backup",
           message="job backup exited 1",
           _raw=f"{t:%b %d %H:%M:%S} web-01 CRON[6{k:03}]: (root) job backup exited 1")

out.close()
print(sum(1 for _ in open(sys.argv[1])), "events written; seeded at", ts(now), "; focus", ts(focus))
