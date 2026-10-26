-- trawl gains investigation_read and investigation_write (ADR-0059, as
-- amended into ADR-0006): reading every investigation in the installation,
-- and writing to any of them.
--
-- Registry only. The app_permissions table is the warn-only vocabulary
-- fleet-admin consults when a role mutation names a permission; it grants
-- nothing. Deliberately NOT added to any existing role: no role is seeded
-- or silently re-permissioned at upgrade. An operator grants them to a role
-- explicitly (`fleet-admin roles add-perm`).
INSERT INTO app_permissions (app, permission)
VALUES ('trawl', 'investigation_read'),
       ('trawl', 'investigation_write')
ON CONFLICT DO NOTHING;
