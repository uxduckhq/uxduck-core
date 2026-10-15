-- S12: controllers and input mappings (ADR-0007), as projections of the
-- event log for inspection. Claims and targets are journaled; these keep the
-- latest.

CREATE TABLE controllers (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    connected INTEGER NOT NULL,
    claim TEXT,
    mode TEXT NOT NULL,
    target TEXT,
    pending_target TEXT,
    mapping_id TEXT NOT NULL,
    version INTEGER NOT NULL,
    updated_seq INTEGER NOT NULL
);

CREATE TABLE input_mappings (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    mapping TEXT NOT NULL,
    version INTEGER NOT NULL,
    updated_seq INTEGER NOT NULL
);
