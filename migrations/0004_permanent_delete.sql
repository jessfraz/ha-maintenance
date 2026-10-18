-- Keep identities reserved so a lost response or migration retry cannot recreate
-- a deliberately deleted item. These records contain no task or completion data.
CREATE TABLE deleted_items (
    id TEXT PRIMARY KEY NOT NULL,
    revision INTEGER NOT NULL CHECK (revision > 0),
    owner_only INTEGER NOT NULL CHECK (owner_only IN (0,1)),
    deleted_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
-- Completion deletion checks the reverse foreign key even after this item's
-- pointer is cleared. Index it so each event does not scan every other item.
CREATE INDEX items_last_completion ON items (last_completion_id)
    WHERE last_completion_id IS NOT NULL;

-- Old item acknowledgements contain completion and checklist history too. Keep
-- their operation identities reserved while allowing that payload to be removed.
ALTER TABLE idempotency ADD COLUMN invalidated INTEGER NOT NULL DEFAULT 0
    CHECK (invalidated IN (0,1));
CREATE INDEX idempotency_item_snapshot
    ON idempotency (json_extract(response,'$.id'))
    WHERE invalidated=0 AND json_type(response,'$.occurrence')='object';
