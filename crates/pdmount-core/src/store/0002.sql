-- Schema version 2 (M2): `inodes.mode`, `mtime_ns` and `size` are the local state of a node.
-- M1 stored 0 as the mode of files without `pdmount.mode`; take the remote mode or the default.
UPDATE inodes SET mode = COALESCE(
    (SELECT remote.mode FROM remote WHERE remote.link_id = inodes.link_id),
    CASE kind WHEN 2 THEN 493 ELSE 420 END)
WHERE mode = 0;
