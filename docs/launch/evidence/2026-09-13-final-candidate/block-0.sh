umask 077
TRAWL_TUTORIAL_DIR=$(mktemp -d /tmp/trawl-tutorial.XXXXXX)
TRAWL_PG_ADMIN_PASSWORD=$(python3 -c 'import secrets; print(secrets.token_hex(24))')
TRAWL_FLEET_PASSWORD=$(python3 -c 'import secrets; print(secrets.token_hex(24))')
TRAWL_APP_PASSWORD=$(python3 -c 'import secrets; print(secrets.token_hex(24))')
printf 'POSTGRES_PASSWORD=%s\n' "$TRAWL_PG_ADMIN_PASSWORD" > "$TRAWL_TUTORIAL_DIR/postgres.env"
docker run --detach --name trawl-docs-postgres \
  --publish 127.0.0.1:55439:5432 \
  --env-file "$TRAWL_TUTORIAL_DIR/postgres.env" postgres:18
