-- Status pages & maintenance extras (statuspages agent).

-- Footer toggle: show "Powered by Vigilant Vole" on the public page.
ALTER TABLE status_pages ADD COLUMN show_powered_by INTEGER NOT NULL DEFAULT 1;

-- Recurring windows ended early with "End now": occurrences that started
-- before this UTC timestamp are skipped.
ALTER TABLE maintenance_windows ADD COLUMN skip_until TEXT;

CREATE INDEX IF NOT EXISTS idx_incidents_page_resolved ON incidents (status_page_id, resolved_at);
