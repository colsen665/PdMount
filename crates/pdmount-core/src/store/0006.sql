-- Schema version 6 (M6.3, D140): when a conflict's desktop notification was accepted by the notification service
-- (unix seconds), so it is shown at most once and one missed while no desktop ran is shown when it appears. Rows
-- from before the migration count as notified: nothing old pops up.
ALTER TABLE conflicts ADD COLUMN notified_at INTEGER NULL;
UPDATE conflicts SET notified_at = CAST(strftime('%s', 'now') AS INTEGER);
