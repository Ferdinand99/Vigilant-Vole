-- HTTP monitor authentication beyond basic auth. `auth_method` is one of
-- 'none' | 'basic' | 'oauth2' | 'mtls'; the columns below only apply to the
-- method they are named for. OAuth2 = client credentials grant;
-- `oauth_client_auth` is 'client_secret_basic' | 'client_secret_post'.
-- The TLS columns hold PEM text (the key and the OAuth client secret are
-- secrets and are masked everywhere they leave the database).
ALTER TABLE monitors ADD COLUMN auth_method TEXT NOT NULL DEFAULT 'none';
ALTER TABLE monitors ADD COLUMN oauth_token_url TEXT;
ALTER TABLE monitors ADD COLUMN oauth_client_id TEXT;
ALTER TABLE monitors ADD COLUMN oauth_client_secret TEXT;
ALTER TABLE monitors ADD COLUMN oauth_scope TEXT;
ALTER TABLE monitors ADD COLUMN oauth_audience TEXT;
ALTER TABLE monitors ADD COLUMN oauth_client_auth TEXT NOT NULL DEFAULT 'client_secret_basic';
ALTER TABLE monitors ADD COLUMN tls_client_cert TEXT;
ALTER TABLE monitors ADD COLUMN tls_client_key TEXT;
ALTER TABLE monitors ADD COLUMN tls_ca_cert TEXT;

-- Monitors that had basic auth configured keep sending it.
UPDATE monitors SET auth_method = 'basic'
 WHERE basic_auth_user IS NOT NULL AND basic_auth_user <> '';
