-- S3: threads and their items as projections of the event log.

CREATE TABLE threads (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    operator_id TEXT,
    model TEXT NOT NULL,
    auto_approve INTEGER NOT NULL,
    created_at_us INTEGER NOT NULL,
    updated_at_us INTEGER NOT NULL,
    version INTEGER NOT NULL,
    updated_seq INTEGER NOT NULL
) STRICT;

CREATE TABLE items (
    id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL,
    position INTEGER NOT NULL,
    kind TEXT NOT NULL,
    status TEXT NOT NULL,
    text TEXT NOT NULL,
    -- The tool call and its receipt as JSON, for tool call items.
    tool_call TEXT,
    created_at_us INTEGER NOT NULL,
    version INTEGER NOT NULL,
    updated_seq INTEGER NOT NULL
) STRICT;

CREATE INDEX items_by_thread ON items (thread_id, position);
