-- Rebuild nullable facts without changing existing occurrence or event identities.
-- Deferring references permits replacing the mutually referencing tables atomically.
PRAGMA defer_foreign_keys = ON;
ALTER TABLE items RENAME TO previous_items;
ALTER TABLE completions RENAME TO previous_completions;
DROP INDEX items_due;
DROP INDEX completions_item_sequence;
CREATE TABLE items (
    id TEXT PRIMARY KEY NOT NULL,
    definition TEXT NOT NULL CHECK (json_valid(definition)),
    revision INTEGER NOT NULL CHECK (revision > 0),
    archived INTEGER NOT NULL DEFAULT 0 CHECK (archived IN (0,1)),
    status TEXT NOT NULL DEFAULT 'needs_action' CHECK (status IN ('needs_action','completed')),
    source TEXT CHECK (source IS NULL OR json_valid(source)),
    source_key TEXT UNIQUE,
    occurrence_id TEXT NOT NULL UNIQUE,
    due TEXT,
    sort_due TEXT GENERATED ALWAYS AS (coalesce(due,'9999-12-31')) STORED,
    checklist TEXT NOT NULL CHECK (json_valid(checklist)),
    last_completion_id INTEGER REFERENCES completions(sequence),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE INDEX items_due ON items (archived,sort_due,id);
CREATE INDEX items_active_due ON items (archived,status,sort_due,id);
CREATE TABLE completions (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE,
    item_id TEXT NOT NULL REFERENCES items(id),
    occurrence_id TEXT UNIQUE,
    completed_at TEXT,
    actor TEXT CHECK (actor IS NULL OR json_valid(actor)),
    previous_due TEXT,
    next_due TEXT,
    checklist TEXT NOT NULL CHECK (json_valid(checklist)),
    source TEXT CHECK (source IS NULL OR json_valid(source)),
    source_key TEXT UNIQUE
);
CREATE INDEX completions_item_sequence ON completions (item_id,sequence DESC);
INSERT INTO items(id,definition,revision,archived,occurrence_id,due,checklist,last_completion_id,created_at)
SELECT id,json_set(definition,'$.schedule',json_object('kind','recurring','interval',json_extract(definition,'$.schedule'))),revision,archived,occurrence_id,due,checklist,last_completion_id,created_at FROM previous_items;
INSERT INTO completions(sequence,id,item_id,occurrence_id,completed_at,actor,previous_due,next_due,checklist)
SELECT sequence,id,item_id,occurrence_id,completed_at,actor,previous_due,next_due,checklist FROM previous_completions;
DROP TABLE previous_completions;
DROP TABLE previous_items;
-- These rows hold full Item acknowledgements; reorder receipts have a different shape.
UPDATE idempotency SET response=json_set(response,'$.schedule',json_object('kind','recurring','interval',json_extract(response,'$.schedule')),'$.status','needs_action','$.source',NULL)
WHERE json_type(response,'$.schedule')='object' AND json_type(response,'$.schedule.kind') IS NULL;
CREATE TABLE migration_sources (
    source_key TEXT PRIMARY KEY NOT NULL,
    target_id TEXT NOT NULL UNIQUE,
    fingerprint BLOB NOT NULL
);
