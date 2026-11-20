-- Schema version 5 (M6.0.6, D132 amended): the nodes of the tree whose signatures did not verify when they were last
-- decoded. A row follows the node's `remote` row: written with it, deleted when the node verifies again or its remote
-- row goes. `parts` holds bits (1 name, 2 key, 4 content key), `digest` one token per failed part; an accept on this
-- device keeps the digest it was given for (`accepted_digest`) and holds while the failures stay covered by it.
CREATE TABLE signatures (
    link_id TEXT PRIMARY KEY,
    parts INTEGER NOT NULL,
    signer TEXT NOT NULL,
    digest TEXT NOT NULL,
    first_seen INTEGER NOT NULL,
    accepted_digest TEXT NULL,
    accepted_at INTEGER NULL
);
