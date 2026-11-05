-- dume-name: STAR-CCM+ drop solution (placeholder, admin must edit)
-- dume-apply: starccm-drop-solution.sh
-- Recipe notes: the view is called `entries` (path, parent, name, is_dir, asize, dsize,
-- ino, nlink, mtime, tree_size). The query must return a `path` column.
-- path and name are BLOBs, so use REGEXP (or CAST): name = 'foo' never matches.
SELECT path FROM entries WHERE name REGEXP '\.sim$' AND is_dir = 0
