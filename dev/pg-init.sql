-- Second logical database so the pre-push `test-no-defaults` suite runs in
-- parallel with `test` without sharing sqlx test-database bookkeeping.
CREATE DATABASE fleet_test_nd OWNER fleet;
