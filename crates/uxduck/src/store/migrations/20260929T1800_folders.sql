-- S4: folders, and where each thread is filed, pinned, archived or deleted.

ALTER TABLE threads ADD COLUMN folder_id TEXT;
ALTER TABLE threads ADD COLUMN sort_key TEXT NOT NULL DEFAULT '';
ALTER TABLE threads ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0;
ALTER TABLE threads ADD COLUMN archived INTEGER NOT NULL DEFAULT 0;
ALTER TABLE threads ADD COLUMN deleted_at_us INTEGER;

CREATE TABLE folders (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    parent_id TEXT,
    operator_id TEXT,
    sort_key TEXT NOT NULL,
    created_at_us INTEGER NOT NULL,
    version INTEGER NOT NULL,
    deleted_at_us INTEGER,
    updated_seq INTEGER NOT NULL
) STRICT;
