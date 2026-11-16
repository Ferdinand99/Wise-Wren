-- Target tokens: an API token with the `target:approve` scope may act on
-- exactly one target. This table says which. A token with the scope but no
-- row here (the target is gone, or the token came from the plain API tokens
-- page) is refused everywhere.
CREATE TABLE api_token_targets (
    token_id  INTEGER PRIMARY KEY REFERENCES api_tokens(id) ON DELETE CASCADE,
    target_id INTEGER NOT NULL REFERENCES targets(id) ON DELETE CASCADE
);
CREATE INDEX api_token_targets_target ON api_token_targets (target_id);
