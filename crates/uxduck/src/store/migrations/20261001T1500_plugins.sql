-- S14: installed plugins (ADR-0006), as a projection of the event log.

CREATE TABLE plugins (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    version TEXT NOT NULL,
    path TEXT NOT NULL,
    capabilities TEXT NOT NULL,
    status TEXT NOT NULL,
    detail TEXT,
    updated_seq INTEGER NOT NULL
);
