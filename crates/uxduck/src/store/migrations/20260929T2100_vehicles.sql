-- S5: links and vehicles as projections of the event log.

CREATE TABLE links (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    -- The kind and its settings as JSON, such as {"type":"udp-listen",...}.
    kind TEXT NOT NULL,
    policy TEXT NOT NULL,
    topology TEXT NOT NULL,
    status TEXT NOT NULL,
    created_at_us INTEGER NOT NULL,
    version INTEGER NOT NULL,
    updated_seq INTEGER NOT NULL
) STRICT;

CREATE TABLE vehicles (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    aliases TEXT NOT NULL,
    link_id TEXT NOT NULL,
    peer TEXT NOT NULL,
    system_id INTEGER NOT NULL,
    autopilot TEXT NOT NULL,
    vehicle_type TEXT NOT NULL,
    uid TEXT,
    firmware TEXT,
    presence TEXT NOT NULL,
    saved INTEGER NOT NULL,
    dismissed INTEGER NOT NULL,
    version INTEGER NOT NULL,
    updated_seq INTEGER NOT NULL
) STRICT;

CREATE INDEX vehicles_by_uid ON vehicles (uid);
