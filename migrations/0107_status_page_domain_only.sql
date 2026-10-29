-- 0107: serve only the status page on its custom domain.
--
-- With `custom_domain_only` set, the status page's custom domain answers
-- the page itself (and what the page needs: static files, its own
-- `/status/{slug}` routes, its JSON summary) and 404 for everything else, so
-- the dashboard, sign-in and API are not reachable on the public host.
-- 0 (the default) keeps the previous behaviour: other paths reach the app.
ALTER TABLE status_pages ADD COLUMN custom_domain_only INTEGER NOT NULL DEFAULT 0 CHECK (custom_domain_only IN (0, 1));
