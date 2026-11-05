#!/bin/sh
# dume-name: Clean OpenFOAM case
# dume-pattern: case
# Runs in each matching directory (cwd = the directory, also $1). fd comes from PREFIX/bin.
fd '^[0-9]*$|^polyMesh$' | xargs rm -rf
