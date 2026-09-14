python3 - <<'PYDATA' > "$TRAWL_TUTORIAL_DIR/events.json"
import datetime, json
now = datetime.datetime.now(datetime.timezone.utc).isoformat()
events = [
    {"level": "info", "message": "server started", "duration": 12},
    {"level": "error", "message": "connection refused", "duration": 1500},
    {"level": "warn", "message": "upstream timeout", "duration": 700},
]
for event in events:
    event.update(service="tutorial", host="tutorial-host", timestamp=now)
print(json.dumps(events))
PYDATA
printf 'Authorization: Bearer %s\n' "$(cat "$TRAWL_TUTORIAL_DIR/ingest.token")" \
  > "$TRAWL_TUTORIAL_DIR/ingest.header"
curl --fail --silent --show-error \
  --cacert "$TRAWL_TUTORIAL_DIR/tls/cert.pem" \
  --header @"$TRAWL_TUTORIAL_DIR/ingest.header" \
  --header 'Content-Type: application/json' \
  --data-binary @"$TRAWL_TUTORIAL_DIR/events.json" \
  https://localhost:15514/api/v1/ingest
