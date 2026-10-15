-- S2: operators as a projection of the event log, the last checkpoint, and
-- browser sessions by token hash.

CREATE TABLE operators (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    units TEXT NOT NULL,
    coordinate_format TEXT NOT NULL,
    version INTEGER NOT NULL,
    updated_seq INTEGER NOT NULL
) STRICT;

-- One row: the whole engine state at `seq`, as JSON (ADR-0007).
CREATE TABLE checkpoint (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    seq INTEGER NOT NULL,
    state TEXT NOT NULL,
    written_at_us INTEGER NOT NULL
) STRICT;

CREATE TABLE sessions (
    token_hash TEXT PRIMARY KEY,
    origin TEXT NOT NULL,
    created_at_us INTEGER NOT NULL
) STRICT;

-- How far the projections have applied the event log.
CREATE TABLE projection (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    seq INTEGER NOT NULL
) STRICT;
INSERT INTO projection (id, seq) VALUES (1, 0);
