-- dume-name: ANSA delete surface mesh (placeholder, admin must edit)
-- dume-apply: ansa-delete-surface-mesh.sh
-- Recipe notes: the view is called `entries` (path, parent, name, is_dir, asize, dsize,
-- ino, nlink, mtime, tree_size). The query must return a `path` column.
-- path and name are BLOBs, so use REGEXP (or CAST): name = 'foo' never matches.
SELECT path FROM entries WHERE name REGEXP '\.ansa$' AND is_dir = 0
