-- S10: client identities with their capabilities, and approvals, as
-- projections of the event log (the audit record: who asked, who answered).

CREATE TABLE clients (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,
    executable TEXT,
    capabilities TEXT NOT NULL,
    created_at_us INTEGER NOT NULL,
    version INTEGER NOT NULL,
    updated_seq INTEGER NOT NULL
);

CREATE TABLE approvals (
    id TEXT PRIMARY KEY,
    client_id TEXT NOT NULL,
    client_name TEXT NOT NULL,
    capability TEXT NOT NULL,
    method TEXT NOT NULL,
    summary TEXT NOT NULL,
    state TEXT NOT NULL,
    remember TEXT,
    deadline_us INTEGER NOT NULL,
    created_at_us INTEGER NOT NULL,
    version INTEGER NOT NULL,
    updated_seq INTEGER NOT NULL
);
