-- vole-kit: the product scopes of API tokens (KitConfig::token_scopes),
-- besides the read/write access in api_tokens.scope.
--
-- A token with no rows here is exactly what it was before: read-only or
-- read-and-write. A read-only token with rows carries those scopes; it also
-- reads when one of them implies read.
CREATE TABLE api_token_scopes (
    token_id INTEGER NOT NULL REFERENCES api_tokens(id) ON DELETE CASCADE,
    scope TEXT NOT NULL,
    PRIMARY KEY (token_id, scope)
) WITHOUT ROWID;
