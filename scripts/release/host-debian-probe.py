#!/usr/bin/env python3
"""Secret-safe HTTP and filesystem probes for test-host-debian.sh."""

import datetime
import errno
import http.cookiejar
import json
import os
import ssl
import sys
import urllib.request


API = "https://127.0.0.1:5514"
WEB = "http://127.0.0.1:8090"
ORIGIN = WEB


class ProbeFailure(RuntimeError):
    """Fixed probe assertion message safe to print without credentials."""


def fail(message):
    raise ProbeFailure(message)


def request(opener, url, payload=None, headers=None):
    body = None if payload is None else json.dumps(payload).encode()
    req = urllib.request.Request(url, data=body, headers=headers or {})
    with opener.open(req, timeout=5) as response:
        if response.status != 200:
            fail("unexpected HTTP status")
        return json.load(response)


def ingest(token_file, certificate):
    with open(token_file, encoding="ascii") as handle:
        token = handle.read().strip()
    context = ssl.create_default_context(cafile=certificate)
    opener = urllib.request.build_opener(
        urllib.request.ProxyHandler({}), urllib.request.HTTPSHandler(context=context)
    )
    event = {
        "_time": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "service": "host-ci",
        "message": "owner-only package verification",
    }
    req = urllib.request.Request(
        API + "/api/v1/ingest",
        data=(json.dumps(event) + "\n").encode(),
        headers={"Authorization": "Bearer " + token, "Content-Type": "application/x-ndjson"},
    )
    with opener.open(req, timeout=5) as response:
        if response.status != 200:
            fail("ingest status was not 200")
        result = json.load(response)
    if result.get("accepted") != 1 or result.get("rejected", 0) != 0:
        fail("ingest did not accept exactly one event")


def signin_query(token_file):
    with open(token_file, encoding="ascii") as handle:
        token = handle.read().strip()
    cookies = http.cookiejar.CookieJar()
    opener = urllib.request.build_opener(
        urllib.request.ProxyHandler({}), urllib.request.HTTPCookieProcessor(cookies)
    )
    headers = {"Origin": ORIGIN, "Content-Type": "application/json"}
    identity = request(opener, WEB + "/api/auth/login", {"api_key": token}, headers)
    if not identity.get("name") or not any(c.name == "fleet_session" for c in cookies):
        fail("login omitted identity or session cookie")
    me = request(opener, WEB + "/api/auth/me", headers={"Origin": ORIGIN})
    if me.get("name") != identity["name"]:
        fail("session identity changed")
    result = request(
        opener,
        WEB + "/api/v1/query",
        {"query": 'service="host-ci" last=24h | stats count()'},
        headers,
    )
    if not any(type(value) is int and value == 1 for row in result.get("rows", []) for value in row):
        fail("proxied query did not find the stored event")


def access(action, path):
    try:
        if action == "deny-list":
            os.listdir(path)
        else:
            with open(path, "rb") as handle:
                handle.read(1)
    except OSError as exc:
        if action.startswith("deny-") and exc.errno == errno.EACCES:
            return
        fail("access failed with errno " + str(exc.errno) + ", expected EACCES" if action.startswith("deny-") else "owner could not open path")
    if action.startswith("deny-"):
        fail("denied account opened protected path")


def main():
    action = sys.argv[1]
    if action == "ingest":
        ingest(sys.argv[2], sys.argv[3])
        print("ok ingest accepted=1")
    elif action == "signin-query":
        signin_query(sys.argv[2])
        print("ok signin-query")
    elif action in ("deny-file", "deny-list", "can-read"):
        access(action, sys.argv[2])
        print("ok " + action + (" EACCES" if action.startswith("deny-") else ""))
    else:
        fail("unknown probe action")


if __name__ == "__main__":
    try:
        main()
    except Exception as exc:
        # Only fail() supplies fixed, secret-free messages. urllib and HTTP
        # exceptions may include a credential-bearing URL or response body.
        detail = ": " + str(exc) if isinstance(exc, ProbeFailure) else ""
        print(
            "host probe failed: " + sys.argv[1] + " (" + type(exc).__name__ + ")" + detail,
            file=sys.stderr,
        )
        sys.exit(1)
