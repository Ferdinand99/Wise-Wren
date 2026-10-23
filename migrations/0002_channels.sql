-- Where notifications go. kind is one of vole-kit's providers; config is its
-- JSON (secrets included: it never leaves the server unredacted). language
-- is the language the messages are written in ('' = the instance's).
CREATE TABLE notification_channels (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,
    config TEXT NOT NULL DEFAULT '{}',
    active INTEGER NOT NULL DEFAULT 1,
    language TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
