-- Schema version 3 (M3): the conflict registry (DESIGN.md §10.8) gains the second inode of a conflict and free text.
ALTER TABLE conflicts ADD COLUMN other_ino INTEGER NULL;
ALTER TABLE conflicts ADD COLUMN detail TEXT NULL;
CREATE INDEX conflicts_state ON conflicts(state);
