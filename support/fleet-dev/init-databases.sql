-- Fresh-volume initialization only. The controller validates existing volumes
-- and never assumes this script reruns.
SELECT 'CREATE DATABASE trawl_dev OWNER fleet_dev'
WHERE NOT EXISTS (SELECT FROM pg_database WHERE datname = 'trawl_dev')
\gexec

SELECT 'CREATE DATABASE coastwatch_dev OWNER fleet_dev'
WHERE NOT EXISTS (SELECT FROM pg_database WHERE datname = 'coastwatch_dev')
\gexec
