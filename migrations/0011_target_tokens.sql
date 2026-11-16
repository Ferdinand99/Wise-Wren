-- Target tokens: an API token with the `target:approve` scope may act on
-- exactly one target. This table says which, by the target's identity
-- (server, kind, name) as target_memory does, so that a container that is
-- re-created or left out of one scan doesn't cut the token off for good: the
-- token works again when the target returns. An inventory drops the row
-- once the target has been gone for as long as its settings are kept.
--
-- A token with the scope but no row here (the plain API tokens page makes
-- those) is refused everywhere, and so is one whose target is missing.
CREATE TABLE api_token_targets (
    token_id  INTEGER PRIMARY KEY REFERENCES api_tokens(id) ON DELETE CASCADE,
    server_id INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
    kind      TEXT NOT NULL,
    name      TEXT NOT NULL
);
