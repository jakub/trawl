trawl-admin tls generate --output-dir "$TRAWL_TUTORIAL_DIR/tls"
python3 -c 'import secrets, sys; sys.stdout.buffer.write(secrets.token_bytes(32))' \
  > "$TRAWL_TUTORIAL_DIR/web.cookie"
cat > "$TRAWL_TUTORIAL_DIR/trawld.toml" <<TOML
[server]
http_addr = "127.0.0.1:15514"
tls_cert_path = "$TRAWL_TUTORIAL_DIR/tls/cert.pem"
tls_key_path = "$TRAWL_TUTORIAL_DIR/tls/key.pem"
max_concurrent_queries = 2

[data]
path = "$TRAWL_TUTORIAL_DIR/data"

[auth]
database_url = "postgres://fleet:$TRAWL_FLEET_PASSWORD@127.0.0.1:55439/fleet"

[storage]
database_url = "postgres://trawl:$TRAWL_APP_PASSWORD@127.0.0.1:55439/trawl"

[ingest]
enabled = true
internal_telemetry = false

[web]
bind_addr = "127.0.0.1:18090"
upstream_url = "https://localhost:15514"
public_origins = ["http://localhost:18090"]
cookie_secret_path = "$TRAWL_TUTORIAL_DIR/web.cookie"
allow_insecure_cookies = true
TOML
printf 'env -u FLEET_DATABASE_URL -u TRAWL_DATABASE_URL -u TRAWL_HTTP_ADDR trawld --config %q\n' "$TRAWL_TUTORIAL_DIR/trawld.toml"
