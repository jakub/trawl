-- trawl gains the schema_write permission (ADR-0011 slice B, issue #53):
-- the repin trigger — the first data-mutating schema action — gets its own
-- compile-time variant so a schema-admin role can exist without
-- server_manage.
--
-- Registry only. The app_permissions table is the warn-only vocabulary
-- fleet-admin consults when a role mutation names a permission; it grants
-- nothing. Deliberately NOT added to any existing role: granting a
-- corpus-rewriting capability to standing keys in a migration would be a
-- silent privilege escalation — an operator adds it to a role explicitly
-- (`fleet-admin roles ...`).
INSERT INTO app_permissions (app, permission)
VALUES ('trawl', 'schema_write')
ON CONFLICT DO NOTHING;
