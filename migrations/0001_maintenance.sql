CREATE TABLE items (
    id TEXT PRIMARY KEY NOT NULL,
    definition TEXT NOT NULL CHECK (json_valid(definition)),
    revision INTEGER NOT NULL CHECK (revision > 0),
    archived INTEGER NOT NULL DEFAULT 0 CHECK (archived IN (0, 1)),
    occurrence_id TEXT NOT NULL UNIQUE,
    due TEXT NOT NULL,
    last_completion_id INTEGER REFERENCES completions(sequence),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX items_due ON items (archived, due, id);
CREATE TABLE completions (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE,
    item_id TEXT NOT NULL REFERENCES items(id),
    occurrence_id TEXT NOT NULL UNIQUE,
    completed_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    actor TEXT NOT NULL CHECK (json_valid(actor)),
    previous_due TEXT NOT NULL,
    next_due TEXT NOT NULL
);
CREATE INDEX completions_item_sequence ON completions (item_id, sequence DESC);
CREATE TABLE idempotency (
    actor TEXT NOT NULL,
    key TEXT NOT NULL,
    fingerprint BLOB NOT NULL,
    response TEXT NOT NULL CHECK (json_valid(response)),
    PRIMARY KEY (actor, key)
);
