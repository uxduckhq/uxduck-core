-- S9: command receipts and notifications as projections of the event log,
-- for history queries. The log keeps every version; these keep the latest.

CREATE TABLE receipts (
    id TEXT PRIMARY KEY,
    vehicle_id TEXT NOT NULL,
    command TEXT NOT NULL,
    status TEXT NOT NULL,
    detail TEXT,
    sent_without_recent_heartbeat INTEGER NOT NULL,
    forced INTEGER NOT NULL,
    attempts INTEGER NOT NULL,
    client_id TEXT NOT NULL,
    operator_id TEXT,
    created_at_us INTEGER NOT NULL,
    completed_at_us INTEGER,
    version INTEGER NOT NULL,
    updated_seq INTEGER NOT NULL
);

CREATE INDEX receipts_by_vehicle ON receipts (vehicle_id, created_at_us);

CREATE TABLE notifications (
    id TEXT PRIMARY KEY,
    severity TEXT NOT NULL,
    kind TEXT NOT NULL,
    state TEXT NOT NULL,
    subject TEXT NOT NULL,
    title TEXT NOT NULL,
    body TEXT,
    condition TEXT,
    created_at_us INTEGER NOT NULL,
    version INTEGER NOT NULL,
    updated_seq INTEGER NOT NULL
);
