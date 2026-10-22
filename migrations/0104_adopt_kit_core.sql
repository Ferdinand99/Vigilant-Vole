-- Hands users, api_tokens and settings over to vole-kit's core migrations.
--
-- Our own migrations (0001, 0002, 0060, 0090) created those tables, in
-- exactly the shape kit/0001_core.sql creates them, so the core migration is
-- recorded as applied instead of run. The schema doesn't change. Once this
-- has run, an older Vigilant Vole (0.6.x and before) refuses the database as
-- coming from a newer release (docs/upgrading.md).
INSERT OR IGNORE INTO schema_migrations (name) VALUES ('kit/0001_core.sql');
