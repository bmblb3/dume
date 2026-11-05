-- dume-name: OpenFOAM leftovers (time folders, polyMesh, dynamicCode, processor*, core files)
-- Recipe notes: the view is called `entries` (path, parent, name, is_dir, asize, dsize,
-- ino, nlink, mtime, tree_size). The query must return a `path` column.
-- path and name are BLOBs, so use REGEXP (or CAST): name = 'foo' never matches.
-- WARNING: numeric folder names anywhere below the folder you clean are matched. Run it from a
-- case or project folder and read the path list before confirming.
SELECT path FROM entries
WHERE name REGEXP '^[0-9]+$|^(polyMesh|dynamicCode)$|^processor[0-9]*$|^core'
