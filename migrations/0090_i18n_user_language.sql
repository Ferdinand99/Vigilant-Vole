-- i18n agent: per-user interface language. NULL = follow the site default
-- (Settings → General). Values: 'en', 'nb'.
ALTER TABLE users ADD COLUMN language TEXT;
