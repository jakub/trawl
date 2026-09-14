docker exec -i trawl-docs-postgres psql -U postgres -v ON_ERROR_STOP=1 <<SQL
CREATE ROLE fleet LOGIN PASSWORD '$TRAWL_FLEET_PASSWORD';
CREATE ROLE trawl LOGIN PASSWORD '$TRAWL_APP_PASSWORD';
CREATE DATABASE fleet OWNER fleet;
CREATE DATABASE trawl OWNER trawl;
SQL
export DATABASE_URL="postgres://fleet:$TRAWL_FLEET_PASSWORD@127.0.0.1:55439/fleet"
fleet-admin migrate
