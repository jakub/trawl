for attempt in {1..30}; do
  docker exec trawl-docs-postgres pg_isready -U postgres >/dev/null 2>&1 && break
  sleep 1
done
docker exec trawl-docs-postgres pg_isready -U postgres
