-- dume-name: OpenFOAM leftovers (time folders, polyMesh, dynamicCode, processor*, core files)
-- Recipe notes: the view is called `entries` (path, parent, name, is_dir, asize, dsize,
-- ino, nlink, mtime, tree_size). The query must return a `path` column.
-- path and name are BLOBs, so use REGEXP (or CAST): name = 'foo' never matches.
-- Time folders are numeric names other than `0` (100, 0.5, 1e-05); `0` holds the case inputs
-- and is never selected (nor is `0.orig`).
-- WARNING: numeric folder names anywhere below the folder you clean are matched. Run it from a
-- case or project folder and read the path list before confirming.
SELECT path FROM entries
WHERE name REGEXP '^([1-9][0-9]*|[0-9]*\.[0-9]+([eE]-?[0-9]+)?|[0-9]+[eE]-?[0-9]+)$'
   OR name REGEXP '^(polyMesh|dynamicCode)$|^processor[0-9]*$|^core(\.[0-9]+)?$'
