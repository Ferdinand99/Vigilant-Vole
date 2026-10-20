-- Status page incident updates: a timeline per incident (investigating →
-- identified → monitoring → resolved), each with its own text and time.
CREATE TABLE incident_updates (
    id INTEGER PRIMARY KEY,
    incident_id INTEGER NOT NULL REFERENCES incidents(id) ON DELETE CASCADE,
    status TEXT NOT NULL CHECK (status IN ('investigating', 'identified', 'monitoring', 'resolved')),
    body TEXT NOT NULL DEFAULT '',              -- Markdown subset, like the incident body
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX idx_incident_updates_incident ON incident_updates (incident_id, created_at);
