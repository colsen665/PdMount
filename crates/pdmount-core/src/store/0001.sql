-- Schema version 1 (DESIGN.md §13.2). Columns used only from M2 on get defaults so that
-- M1 can write the subset it knows without a later migration.

CREATE TABLE meta (
    key TEXT PRIMARY KEY,
    value BLOB
);

CREATE TABLE inodes (
    ino INTEGER PRIMARY KEY,
    kind INTEGER NOT NULL,
    parent_ino INTEGER NULL,
    name TEXT NULL,
    link_id TEXT UNIQUE NULL,
    mode INTEGER NOT NULL DEFAULT 0,
    mtime_ns INTEGER NOT NULL DEFAULT 0,
    ctime_ns INTEGER NOT NULL DEFAULT 0,
    atime_ns INTEGER NOT NULL DEFAULT 0,
    size INTEGER NOT NULL DEFAULT 0,
    listed INTEGER NOT NULL DEFAULT 0,
    local_only INTEGER NOT NULL DEFAULT 0,
    deleted INTEGER NOT NULL DEFAULT 0,
    nlookup_persist INTEGER NOT NULL DEFAULT 0,
    content_gen INTEGER NOT NULL DEFAULT 0,
    meta_gen INTEGER NOT NULL DEFAULT 0,
    uploaded_content_gen INTEGER NOT NULL DEFAULT 0,
    uploaded_meta_gen INTEGER NOT NULL DEFAULT 0,
    state INTEGER NOT NULL DEFAULT 0,
    remote_newer INTEGER NOT NULL DEFAULT 0,
    deferred_error INTEGER NULL
);
CREATE UNIQUE INDEX inodes_parent_name ON inodes(parent_ino, name) WHERE parent_ino IS NOT NULL;

CREATE TABLE remote (
    link_id TEXT PRIMARY KEY,
    parent_link TEXT,
    name TEXT,
    name_hash TEXT,
    kind INTEGER,
    active_rev TEXT NULL,
    size INTEGER,
    mtime_ns INTEGER,
    mode INTEGER NULL,
    sha1 BLOB NULL,
    trashed INTEGER NOT NULL DEFAULT 0,
    seen_at INTEGER
);

CREATE TABLE staging (
    ino INTEGER PRIMARY KEY,
    base_rev TEXT NULL,
    size INTEGER,
    materialized BLOB,
    snap_gen INTEGER NULL,
    snap_meta_gen INTEGER NULL
);

CREATE TABLE journal (
    ino INTEGER PRIMARY KEY,
    kind INTEGER,
    state INTEGER,
    attempts INTEGER,
    next_at INTEGER,
    last_error TEXT NULL,
    parked_reason TEXT NULL,
    updated_at INTEGER
);

CREATE TABLE own_revisions (
    link_id TEXT,
    rev TEXT,
    created_at INTEGER,
    PRIMARY KEY (link_id, rev)
);

CREATE TABLE revision_blocks (
    link_id TEXT,
    rev TEXT,
    idx INTEGER,
    plain_size INTEGER,
    sha256 BLOB,
    PRIMARY KEY (link_id, rev, idx)
);

CREATE TABLE cache_blocks (
    link_id TEXT,
    rev TEXT,
    idx INTEGER,
    bytes INTEGER,
    last_access INTEGER,
    pin INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (link_id, rev, idx)
);

CREATE TABLE pins (
    ino INTEGER PRIMARY KEY,
    path_at_set TEXT,
    created_at INTEGER
);

CREATE TABLE conflicts (
    id INTEGER PRIMARY KEY,
    kind TEXT,
    ino INTEGER,
    link_id TEXT NULL,
    orig_name TEXT,
    conflict_name TEXT NULL,
    local_rev TEXT NULL,
    remote_rev TEXT NULL,
    local_device TEXT,
    remote_device TEXT NULL,
    created_at INTEGER,
    state TEXT
);

CREATE TABLE event_cursor (
    volume TEXT PRIMARY KEY,
    event_id TEXT,
    updated_at INTEGER
);

CREATE TABLE access_rules (
    id INTEGER PRIMARY KEY,
    ino INTEGER UNIQUE,
    link_id TEXT NULL,
    path_at_set TEXT,
    mode TEXT CHECK (mode IN ('ro', 'rw')),
    source TEXT CHECK (source IN ('cli', 'config')),
    status TEXT CHECK (status IN ('active', 'orphaned')),
    created_at INTEGER,
    updated_at INTEGER
);
