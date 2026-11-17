-- Schema version 4 (M5.6, D105): when each journal entry was queued, so `status` counts its age across restarts.
-- Rows exist only for entries that are queued; the journal itself is still derived from the inode rows at mount.
CREATE TABLE queued (
    ino INTEGER PRIMARY KEY,
    queued_at_ms INTEGER NOT NULL
);
