-- Requests deliberately have no foreign key to items: removing or changing a
-- source task must not erase an acknowledged hardware action.
CREATE TABLE reorders (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE,
    receipt TEXT NOT NULL CHECK (json_valid(receipt)),
    status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'fulfilled', 'dismissed')),
    revision INTEGER NOT NULL DEFAULT 1 CHECK (revision > 0),
    resolved_at TEXT,
    resolved_by TEXT CHECK (resolved_by IS NULL OR json_valid(resolved_by)),
    CHECK ((status = 'pending' AND resolved_at IS NULL AND resolved_by IS NULL)
        OR (status != 'pending' AND resolved_at IS NOT NULL AND resolved_by IS NOT NULL))
);
CREATE INDEX reorders_status_sequence ON reorders (status, sequence);
