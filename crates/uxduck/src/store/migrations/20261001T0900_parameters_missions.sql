-- S11: parameter snapshots and missions per vehicle (ADR-0007), as
-- projections of the event log. The log keeps every change; these keep the
-- latest, for history queries and inspection.

CREATE TABLE parameter_sets (
    vehicle_id TEXT PRIMARY KEY,
    status TEXT NOT NULL,
    detail TEXT,
    count INTEGER NOT NULL,
    boot_instant_us INTEGER,
    synced_at_us INTEGER,
    updated_seq INTEGER NOT NULL
);

CREATE TABLE parameters (
    vehicle_id TEXT NOT NULL,
    name TEXT NOT NULL,
    value REAL NOT NULL,
    kind TEXT NOT NULL,
    updated_seq INTEGER NOT NULL,
    PRIMARY KEY (vehicle_id, name)
);

CREATE TABLE missions (
    vehicle_id TEXT PRIMARY KEY,
    items TEXT NOT NULL,
    status TEXT NOT NULL,
    detail TEXT,
    boot_instant_us INTEGER,
    synced_at_us INTEGER,
    version INTEGER NOT NULL,
    updated_seq INTEGER NOT NULL
);
